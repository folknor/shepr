use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
#[cfg(test)]
use ratatui::style::Modifier;
use ratatui::style::{Color, Style};
use ratatui::{Frame, layout::Rect};
#[cfg(test)]
use serde::{Deserialize, Serialize};
use tracing::{debug, error};
use unicode_width::UnicodeWidthStr;

use crate::layout::PaneId;
use crate::protocol::CellData;
use crate::terminal::{AbsRow, Point, ScreenRow, ViewportRow};

#[cfg(test)]
mod migration_tests;

use super::cursor::decscusr_cursor_shape;
use super::osc::{
    AgentOscStateTracker, OscDebugTracker, current_transient_default_color_owner,
    parse_reported_cwd, restore_host_terminal_theme_if_needed,
};

const DEFAULT_DETECTION_ROWS: usize = 24;
/// Slack after a synchronized update's deadline before the follow-up render.
const SYNCHRONIZED_OUTPUT_FLUSH_MARGIN: Duration = Duration::from_millis(5);
const MODE_MOUSE_X10: u16 = 9;
const MODE_MOUSE_PRESS_RELEASE: u16 = 1000;
const MODE_MOUSE_BUTTON_MOTION: u16 = 1002;
const MODE_MOUSE_ANY_MOTION: u16 = 1003;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollMetrics {
    pub offset_from_bottom: usize,
    pub max_offset_from_bottom: usize,
    pub viewport_rows: usize,
    pub history_origin: crate::terminal::AbsRow,
}

impl ScrollMetrics {
    /// The stable row ID at the top of the current viewport.
    pub fn viewport_top_row(self) -> crate::terminal::AbsRow {
        let screen_row = self
            .max_offset_from_bottom
            .saturating_sub(self.offset_from_bottom);
        self.history_origin
            .saturating_add(u64::try_from(screen_row).unwrap_or(u64::MAX))
    }

    /// Convert a viewport-relative row to its stable row ID.
    pub fn absolute_row_at_viewport(
        self,
        row: crate::terminal::ViewportRow,
    ) -> crate::terminal::AbsRow {
        crate::terminal::AbsRow::from_viewport_top(self.viewport_top_row(), row)
    }
}

/// Scroll metrics together with the row origin read under one terminal lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPosition {
    pub metrics: ScrollMetrics,
}

impl ScrollPosition {
    #[cfg(test)]
    pub fn viewport_top_row(self) -> crate::terminal::AbsRow {
        self.metrics.viewport_top_row()
    }
}

/// A cell position in terminal text. `R` distinguishes the retained-buffer
/// index from a stable absolute row identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TerminalTextPoint<R = ScreenRow> {
    pub row: R,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalTextMatch<R = ScreenRow> {
    pub start: TerminalTextPoint<R>,
    pub end: TerminalTextPoint<R>,
    pub source_fingerprint: u64,
    pub scan_cols: u16,
    pub scan_screen: crate::ghostty::ActiveScreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalSearchDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalSearchWindow<R = ScreenRow> {
    pub matches: Vec<TerminalTextMatch<R>>,
    pub current: Option<usize>,
    pub current_global: Option<usize>,
    pub total: usize,
}

impl<R> TerminalSearchWindow<R> {
    fn empty() -> Self {
        Self {
            matches: Vec::new(),
            current: None,
            current_global: None,
            total: 0,
        }
    }
}

/// Rows a chunked history scan reads per hold of the terminal lock. Between
/// chunks the lock is released so the PTY reader, rendering and detection
/// are never stalled behind a scan of the whole scrollback.
const SCAN_CHUNK_ROWS: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalWordMotion {
    NextStart,
    PreviousStart,
    NextEnd,
    NextBigStart,
    PreviousBigStart,
    NextBigEnd,
}

const COPY_MODE_WORD_SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    /// DECSCUSR cursor shape, or the terminal default.
    pub shape: crate::protocol::CursorShapeParam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneClearError {
    TerminalLockPoisoned,
}

impl std::fmt::Display for PaneClearError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TerminalLockPoisoned => f.write_str("terminal lock poisoned"),
        }
    }
}

impl std::error::Error for PaneClearError {}

impl From<PaneClearError> for String {
    fn from(value: PaneClearError) -> Self {
        value.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalDirtyPatch {
    pub rows: Vec<(u16, Vec<CellData>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalDirtyPatchOutcome {
    Clean,
    Patch(TerminalDirtyPatch),
    Fallback,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputState {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub mouse_protocol_mode: crate::input::MouseProtocolMode,
    pub mouse_protocol_encoding: crate::input::MouseProtocolEncoding,
    pub mouse_alternate_scroll: bool,
    #[serde(default)]
    pub modify_other_keys: bool,
    #[serde(default)]
    pub color_scheme_reporting: bool,
}

#[cfg(test)]
impl InputState {
    pub fn mouse_reporting_enabled(self) -> bool {
        self.mouse_protocol_mode.reporting_enabled()
    }

    pub fn plain_page_keys_use_host_scrollback(self) -> bool {
        !self.alternate_screen
            && !self.mouse_reporting_enabled()
            // Bracketed paste distinguishes zsh's line editor (where it's on)
            // from e.g. less -X (where it's off).
            && (!self.application_cursor || self.bracketed_paste)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ProcessBytesResult {
    pub request_render: bool,
    pub render_delay: Option<Duration>,
    pub terminal_title_changed: bool,
    pub clipboard_writes: Vec<Vec<u8>>,
    pub reported_cwd: Option<std::path::PathBuf>,
    pub terminal_responses: Vec<Bytes>,
    pub default_color_owner_pending: bool,
    pub default_color_generation: u64,
    /// The core lock was poisoned: a panic on another thread (render,
    /// detection, an API read) while it held the lock. The bytes were not
    /// processed and no later bytes will be; the reader must end the pane.
    pub core_poisoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TerminalReadSnapshot {
    pub text: String,
    pub truncated: bool,
}

pub(crate) struct GhosttyPaneTerminal {
    /// Poisoned for good once anything panics while holding it. The readers
    /// below then answer empty or default values rather than error: the PTY
    /// actor checks `is_poisoned` on every loop (idle polls included, so at
    /// least once a second) and ends the pane, which is reported dead and
    /// removed, so those answers only cover that short window. Turning every
    /// reader into a fallible one would push a `Result` through render,
    /// detection and the API for a state that lasts under a second.
    pub core: Mutex<GhosttyPaneCore>,
}

pub(crate) struct GhosttyPaneCore {
    #[cfg(test)]
    pub dirty_collection_hook: Option<Box<dyn FnOnce() + Send>>,
    pub terminal: crate::ghostty::Terminal,
    synchronized_output_epoch: u64,
    pub render_state: crate::ghostty::RenderState,
    pub initial_default_foreground: Option<crate::ghostty::RgbColor>,
    pub initial_default_background: Option<crate::ghostty::RgbColor>,
    pub host_terminal_theme: crate::host_term::theme::TerminalTheme,
    /// Process group of the foreground program that last overrode a default
    /// colour (OSC 10/11); its overrides are dropped once the shell is back
    /// in the foreground. `None` while no override is in effect.
    pub transient_default_color_owner_pgid: Option<u32>,
    default_color_generation: u64,
    pub osc_debug_tracker: OscDebugTracker,
    pub agent_osc_state: AgentOscStateTracker,
}

pub(crate) struct PaneTerminal {
    pub(crate) ghostty: GhosttyPaneTerminal,
}

impl PaneTerminal {
    pub(crate) fn new(ghostty: GhosttyPaneTerminal) -> Self {
        Self { ghostty }
    }

    /// Whether a panic while holding the core lock has broken the core. A
    /// single atomic load, taking no lock: the PTY actor asks on every loop.
    pub(crate) fn core_poisoned(&self) -> bool {
        crate::ghostty::terminal_core_is_poisoned(&self.ghostty.core)
    }

    pub fn process_pty_bytes(
        &self,
        pane_id: PaneId,
        shell_pid: u32,
        bytes: &[u8],
    ) -> ProcessBytesResult {
        self.ghostty.process_pty_bytes(pane_id, shell_pid, bytes)
    }

    pub(crate) fn resolve_default_color_owner(
        &self,
        pane_id: PaneId,
        shell_pid: u32,
        generation: u64,
    ) {
        self.ghostty
            .resolve_default_color_owner(pane_id, shell_pid, generation);
    }

    /// See [`GhosttyPaneTerminal::flush_expired_synchronized_output`]. The
    /// reader's `render_delay` timer should call this and deliver the result
    /// like a PTY read's (replies to the child, clipboard writes, cwd, title).
    pub(crate) fn flush_expired_synchronized_output(
        &self,
        pane_id: PaneId,
        shell_pid: u32,
    ) -> ProcessBytesResult {
        self.ghostty
            .flush_expired_synchronized_output(pane_id, shell_pid)
    }

    pub fn resize(&self, geometry: crate::geometry::PaneGeometry) -> Vec<Bytes> {
        self.ghostty.resize(geometry)
    }

    pub fn scroll_up(&self, lines: usize) {
        self.ghostty.scroll_up(lines);
    }

    pub fn scroll_down(&self, lines: usize) {
        self.ghostty.scroll_down(lines);
    }

    pub fn scroll_reset(&self) {
        self.ghostty.scroll_reset();
    }

    pub fn clear_screen(&self) -> Result<(), PaneClearError> {
        self.ghostty.clear_screen()
    }

    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.ghostty.set_scroll_offset_from_bottom(lines);
    }

    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        self.ghostty.scroll_metrics()
    }

    /// Copy-mode search with screen rows. Screen rows shift once history at
    /// its limit evicts lines; [`PaneTerminal::search_text_window_absolute`]
    /// takes and returns absolute rows, which do not.
    pub(crate) fn search_text_window(
        &self,
        query: &str,
        case_sensitive: bool,
        direction: TerminalSearchDirection,
        cursor: TerminalTextPoint,
        previous: Option<(TerminalTextPoint, TerminalTextPoint)>,
        limit: usize,
    ) -> TerminalSearchWindow {
        let Some(origin) = self.ghostty.history_origin() else {
            return TerminalSearchWindow::empty();
        };
        let window = self.ghostty.search_text_window(
            query,
            case_sensitive,
            direction,
            absolute_point(cursor, origin),
            previous
                .map(|(start, end)| (absolute_point(start, origin), absolute_point(end, origin))),
            limit,
        );
        TerminalSearchWindow {
            matches: window
                .matches
                .into_iter()
                .map(|text_match| TerminalTextMatch {
                    start: screen_point(text_match.start, origin),
                    end: screen_point(text_match.end, origin),
                    source_fingerprint: text_match.source_fingerprint,
                    scan_cols: text_match.scan_cols,
                    scan_screen: text_match.scan_screen,
                })
                .collect(),
            current: window.current,
            current_global: window.current_global,
            total: window.total,
        }
    }

    /// Word motion with screen rows; see
    /// [`PaneTerminal::word_motion_target_absolute`].
    pub(crate) fn word_motion_target(
        &self,
        row: ScreenRow,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint> {
        let core = crate::ghostty::lock_terminal_core(&self.ghostty.core).ok()?;
        let origin = core.terminal.history_origin();
        let target = word_motion_in(
            &core.terminal,
            absolute_point(TerminalTextPoint { row, col }, origin),
            motion,
        )?;
        Some(screen_point(target, origin))
    }

    pub(crate) fn dimensions(&self) -> Option<(u16, u16)> {
        let core = crate::ghostty::lock_terminal_core(&self.ghostty.core).ok()?;
        Some((core.terminal.cols(), core.terminal.rows()))
    }

    /// Paragraph motion with screen rows; see
    /// [`PaneTerminal::paragraph_motion_target_absolute`].
    pub(crate) fn paragraph_motion_target(
        &self,
        row: ScreenRow,
        direction: i8,
    ) -> Option<TerminalTextPoint> {
        let core = crate::ghostty::lock_terminal_core(&self.ghostty.core).ok()?;
        let origin = core.terminal.history_origin();
        let absolute = row.absolute(origin);
        let target = paragraph_motion_in(&core.terminal, absolute, direction)?;
        Some(screen_point(target, origin))
    }

    #[cfg(test)]
    pub fn input_state(&self) -> Option<InputState> {
        self.ghostty.input_state()
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.ghostty.bracketed_paste_enabled()
    }

    pub fn focus_reporting_enabled(&self) -> bool {
        self.ghostty.focus_reporting_enabled()
    }

    pub fn mouse_reporting_enabled(&self) -> bool {
        self.ghostty.mouse_reporting_enabled()
    }

    pub fn modify_other_keys_level(&self) -> u8 {
        self.ghostty.modify_other_keys_level()
    }

    pub fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.ghostty.sgr_pixel_mouse_enabled()
    }

    pub fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.ghostty.plain_page_keys_use_host_scrollback()
    }

    pub fn alternate_screen_active(&self) -> bool {
        self.ghostty.alternate_screen_active()
    }

    pub fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.ghostty.wheel_routing()
    }

    pub(crate) fn screen_text_snapshot(
        &self,
    ) -> Option<(
        crate::ghostty::ActiveScreen,
        u16,
        Vec<crate::ghostty::ScreenTextRow>,
    )> {
        self.ghostty.screen_text_snapshot()
    }

    pub fn cursor_state(&self) -> Option<TerminalCursorState> {
        self.ghostty.cursor_state()
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.ghostty.synchronized_output_active()
    }

    pub(crate) fn synchronized_output_state(&self) -> (bool, u64) {
        self.ghostty.synchronized_output_state()
    }

    pub fn visible_text(&self) -> String {
        self.ghostty.visible_text()
    }

    pub fn visible_ansi(&self) -> String {
        self.ghostty.visible_ansi()
    }

    pub fn detection_text(&self) -> String {
        self.ghostty.detection_text()
    }

    pub(crate) fn recent_text_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.ghostty.recent_text_snapshot(lines)
    }

    pub(crate) fn recent_ansi_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.ghostty.recent_ansi_snapshot(lines)
    }

    pub(crate) fn recent_unwrapped_text_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.ghostty.recent_unwrapped_text_snapshot(lines)
    }

    pub(crate) fn recent_unwrapped_ansi_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        self.ghostty.recent_unwrapped_ansi_snapshot(lines)
    }

    /// The selected text, read by stable row identity. Returns `None` if
    /// either row has been evicted from terminal history.
    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        self.ghostty.extract_selection(selection)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        self.ghostty.render(frame, area, show_cursor);
    }

    pub fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        self.ghostty.collect_dirty_patch(area_width, area_height)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.ghostty.visible_hyperlinks(area)
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::host_term::theme::TerminalTheme) {
        self.ghostty.apply_host_terminal_theme(theme);
    }

    pub fn apply_host_terminal_appearance(
        &self,
        appearance: Option<crate::host_term::theme::HostAppearance>,
    ) -> Option<Bytes> {
        self.ghostty.apply_host_terminal_appearance(appearance)
    }

    pub fn has_transient_default_color_override(&self) -> bool {
        self.ghostty.has_transient_default_color_override()
    }

    pub fn maybe_restore_host_terminal_theme(&self, pane_id: PaneId, shell_pid: u32) -> bool {
        self.ghostty
            .maybe_restore_host_terminal_theme(pane_id, shell_pid)
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.ghostty.terminal_title()
    }

    pub fn agent_osc_title(&self) -> String {
        self.ghostty.agent_osc_title()
    }

    pub fn agent_osc_progress(&self) -> String {
        self.ghostty.agent_osc_progress()
    }

    /// Clears retained OSC title/progress evidence on foreground agent change.
    pub fn clear_agent_osc_state(&self) {
        self.ghostty.clear_agent_osc_state();
    }

    pub fn keyboard_protocol(
        &self,
        fallback: crate::input::KeyboardProtocol,
    ) -> crate::input::KeyboardProtocol {
        self.ghostty.keyboard_protocol().unwrap_or(fallback)
    }

    pub fn encode_terminal_key(
        &self,
        key: crate::input::TerminalKey,
        protocol: crate::input::KeyboardProtocol,
    ) -> Vec<u8> {
        self.ghostty.encode_terminal_key(key, protocol)
    }

    pub(crate) fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.ghostty.encode_mouse_button(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.ghostty.encode_mouse_motion(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.ghostty.encode_mouse_wheel(kind, position, modifiers)
    }
}

/// Direct readers addressed by stable absolute row IDs, plus primary history
/// reads for session persistence.
// The copy-search and copy-motion endpoints use screen rows. These helpers
// keep the absolute-row operations directly testable without scroll metrics.
#[allow(dead_code)]
impl PaneTerminal {
    /// The viewport position and the absolute row id of screen row 0, read
    /// together; see [`ScrollPosition`].
    pub fn scroll_position(&self) -> Option<ScrollPosition> {
        self.ghostty.scroll_position()
    }

    /// Copy-mode search over the retained text, with absolute rows. The scan
    /// releases the terminal lock between chunks of rows; output arriving
    /// meanwhile can leave the result inconsistent, which the caller detects
    /// through the pane's content revision like any other read racing
    /// output.
    pub(crate) fn search_text_window_absolute(
        &self,
        query: &str,
        case_sensitive: bool,
        direction: TerminalSearchDirection,
        cursor: TerminalTextPoint<AbsRow>,
        previous: Option<(TerminalTextPoint<AbsRow>, TerminalTextPoint<AbsRow>)>,
        limit: usize,
    ) -> TerminalSearchWindow<AbsRow> {
        self.ghostty
            .search_text_window(query, case_sensitive, direction, cursor, previous, limit)
    }

    /// Where a copy-mode word motion from `row`/`col` lands, with absolute
    /// rows. `None` when the row is no longer retained.
    pub(crate) fn word_motion_target_absolute(
        &self,
        row: AbsRow,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint<AbsRow>> {
        let core = crate::ghostty::lock_terminal_core(&self.ghostty.core).ok()?;
        word_motion_in(&core.terminal, TerminalTextPoint { row, col }, motion)
    }

    /// The next blank row above (`direction < 0`) or below `row`, with
    /// absolute rows, looking at most 1000 rows away. `None` when the row is
    /// no longer retained.
    pub(crate) fn paragraph_motion_target_absolute(
        &self,
        row: AbsRow,
        direction: i8,
    ) -> Option<TerminalTextPoint<AbsRow>> {
        let core = crate::ghostty::lock_terminal_core(&self.ghostty.core).ok()?;
        paragraph_motion_in(&core.terminal, row, direction)
    }

    /// The whole primary-screen history as unwrapped ANSI, for session
    /// persistence. `None` while the alternate screen is active: alacritty
    /// gives no access to the inactive primary grid, and the active grid then
    /// holds the full-screen program's frame, which must not overwrite the
    /// history saved earlier.
    pub fn primary_history_ansi(&self) -> Option<String> {
        self.ghostty.primary_history_ansi()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextClass {
    Whitespace,
    Separator,
    Word,
}

/// A word-motion unit: one cell's text (or a line break when `point` is
/// `None`). Rows are absolute.
#[derive(Debug)]
struct TextAtom {
    point: Option<TerminalTextPoint<AbsRow>>,
    end_col: u16,
    class: TextClass,
}

/// Where one cell's text sits in a [`LogicalTextLine`]. Rows are absolute.
#[derive(Debug)]
struct TextSpan {
    byte_start: usize,
    byte_end: usize,
    start: TerminalTextPoint<AbsRow>,
    end: TerminalTextPoint<AbsRow>,
}

/// The text of one hard line (soft-wrapped rows joined), trailing blanks
/// trimmed, with the cell each byte range came from.
#[derive(Debug, Default)]
struct LogicalTextLine {
    text: String,
    spans: Vec<TextSpan>,
}

impl LogicalTextLine {
    fn clear(&mut self) {
        self.text.clear();
        self.spans.clear();
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.spans.is_empty()
    }

    fn trim_end(&mut self) {
        let trimmed_len = self.text.trim_end().len();
        while self
            .spans
            .last()
            .is_some_and(|span| span.byte_start >= trimmed_len)
        {
            self.spans.pop();
        }
        self.text.truncate(trimmed_len);
    }
}

/// Assembles terminal cells, fed row by row, into logical lines (for search)
/// and word atoms (for word motion). Cells arrive as text, so the live
/// terminal can feed it straight from its grid without building per-cell
/// copies, and lines are handed over one at a time as they complete, so a
/// search never holds the whole history's text.
struct TextBufferBuilder {
    build_lines: bool,
    build_atoms: bool,
    line: LogicalTextLine,
    /// `line` holds a completed line the reader has seen; the next cell
    /// starts a new one.
    line_complete: bool,
    atoms: Vec<TextAtom>,
}

impl TextBufferBuilder {
    fn new(build_lines: bool, build_atoms: bool) -> Self {
        Self {
            build_lines,
            build_atoms,
            line: LogicalTextLine::default(),
            line_complete: false,
            atoms: Vec::new(),
        }
    }

    fn push_cell(&mut self, row: AbsRow, col: u16, wide: crate::ghostty::CellWide, text: &str) {
        if self.line_complete {
            self.line.clear();
            self.line_complete = false;
        }
        match wide {
            crate::ghostty::CellWide::SpacerTail => {}
            crate::ghostty::CellWide::SpacerHead => {
                // The blank a wide character leaves at a soft wrap belongs to
                // the word around it, and to no text.
                if self.build_atoms {
                    let class = self
                        .atoms
                        .last()
                        .map_or(TextClass::Whitespace, |atom| atom.class);
                    self.atoms.push(TextAtom {
                        point: Some(TerminalTextPoint { row, col }),
                        end_col: col,
                        class,
                    });
                }
            }
            crate::ghostty::CellWide::Narrow | crate::ghostty::CellWide::Wide => {
                let width = if wide == crate::ghostty::CellWide::Wide {
                    2
                } else {
                    1
                };
                let start = TerminalTextPoint { row, col };
                let end = TerminalTextPoint {
                    row,
                    col: col.saturating_add(width - 1),
                };
                if self.build_lines {
                    let byte_start = self.line.text.len();
                    self.line.text.push_str(text);
                    let byte_end = self.line.text.len();
                    self.line.spans.push(TextSpan {
                        byte_start,
                        byte_end,
                        start,
                        end,
                    });
                }
                if self.build_atoms {
                    self.atoms.push(TextAtom {
                        point: Some(start),
                        end_col: end.col,
                        class: text_class(text),
                    });
                }
            }
        }
    }

    /// Ends a row. Returns whether it completed a logical line, which is then
    /// in `self.line` until the next cell arrives.
    fn end_row(&mut self, soft_wrapped: bool) -> bool {
        if soft_wrapped {
            return false;
        }
        if self.build_lines {
            self.line.trim_end();
            self.line_complete = true;
        }
        if self.build_atoms {
            self.atoms.push(TextAtom {
                point: None,
                end_col: 0,
                class: TextClass::Whitespace,
            });
        }
        true
    }

    /// Forgets a partially assembled line (its first rows were evicted
    /// while a chunked scan had the lock released).
    fn discard_line(&mut self) {
        self.line.clear();
        self.line_complete = false;
    }

    /// The text of rows after the last hard line break (the buffer ended on
    /// a soft-wrapped row), untrimmed, if there is any.
    fn trailing_line(&self) -> Option<&LogicalTextLine> {
        (self.build_lines && !self.line_complete && !self.line.is_empty()).then_some(&self.line)
    }
}

/// Word atoms over a window of rows (and, for tests, the logical lines).
#[derive(Debug)]
struct RetainedTextBuffer {
    atoms: Vec<TextAtom>,
    #[cfg(test)]
    cols: u16,
    #[cfg(test)]
    lines: Vec<LogicalTextLine>,
}

impl RetainedTextBuffer {
    /// A buffer over owned rows 0.., for exercising search and word motion
    /// on hand-built cells; the live terminal streams straight from its grid.
    #[cfg(test)]
    fn new(cols: u16, rows: Vec<crate::ghostty::ScreenTextRow>) -> Self {
        let mut builder = TextBufferBuilder::new(true, true);
        let mut lines = Vec::new();
        for (row, screen_row) in (0u64..).zip(rows) {
            let row = AbsRow(row);
            for (col, cell) in (0u16..).zip(&screen_row.cells) {
                builder.push_cell(row, col, cell.wide, &terminal_cell_text(&cell.graphemes));
            }
            if builder.end_row(screen_row.soft_wrapped) {
                lines.push(std::mem::take(&mut builder.line));
            }
        }
        if builder.trailing_line().is_some() {
            lines.push(std::mem::take(&mut builder.line));
        }
        Self {
            atoms: builder.atoms,
            cols,
            lines,
        }
    }

    /// Word atoms for screen rows `start..end` of the live terminal, with
    /// absolute rows, plus how the first and last row wrap.
    fn live_words(
        terminal: &crate::ghostty::Terminal,
        start: usize,
        end: usize,
    ) -> Option<(Self, crate::ghostty::RowWrap, crate::ghostty::RowWrap)> {
        let mut builder = TextBufferBuilder::new(false, true);
        let mut scratch = String::new();
        let mut first = None;
        let mut last = crate::ghostty::RowWrap::default();
        for y in start..end {
            let screen_row = ScreenRow(y);
            let row = terminal.absolute_row_for_screen(screen_row);
            let wrap =
                terminal.visit_screen_row_text(screen_row, &mut scratch, |col, wide, text| {
                    builder.push_cell(row, col, wide, text);
                })?;
            builder.end_row(wrap.soft_wrapped);
            if first.is_none() {
                first = Some(wrap);
            }
            last = wrap;
        }
        let buffer = Self {
            atoms: builder.atoms,
            #[cfg(test)]
            cols: terminal.cols(),
            #[cfg(test)]
            lines: Vec::new(),
        };
        Some((buffer, first.unwrap_or_default(), last))
    }

    #[cfg(test)]
    fn search_window(
        &self,
        query: &str,
        case_sensitive: bool,
        active_screen: crate::ghostty::ActiveScreen,
        direction: TerminalSearchDirection,
        cursor: TerminalTextPoint<AbsRow>,
        previous: Option<(TerminalTextPoint<AbsRow>, TerminalTextPoint<AbsRow>)>,
        limit: usize,
    ) -> TerminalSearchWindow<AbsRow> {
        let Some(mut search) =
            TextSearch::new(query, case_sensitive, direction, cursor, previous, limit)
        else {
            return TerminalSearchWindow::empty();
        };
        for line in &self.lines {
            search.scan_line(line, self.cols, active_screen);
        }
        search.finish()
    }

    fn word_motion(
        &self,
        row: AbsRow,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint<AbsRow>> {
        let current = self.atoms.iter().position(|atom| {
            atom.point
                .is_some_and(|point| point.row == row && col >= point.col && col <= atom.end_col)
        })?;
        match motion {
            TerminalWordMotion::NextStart => self.next_word_start(current),
            TerminalWordMotion::PreviousStart => self.previous_word_start(current),
            TerminalWordMotion::NextEnd => self.next_word_end(current),
            TerminalWordMotion::NextBigStart => self.next_big_word_start(current),
            TerminalWordMotion::PreviousBigStart => self.previous_big_word_start(current),
            TerminalWordMotion::NextBigEnd => self.next_big_word_end(current),
        }
    }

    fn next_word_start(&self, current: usize) -> Option<TerminalTextPoint<AbsRow>> {
        let current_class = self.atoms.get(current)?.class;
        let mut next = current.saturating_add(1);
        if current_class != TextClass::Whitespace {
            while self
                .atoms
                .get(next)
                .is_some_and(|atom| atom.class == current_class)
            {
                next += 1;
            }
        }
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        self.next_point(next)
    }

    fn previous_word_start(&self, current: usize) -> Option<TerminalTextPoint<AbsRow>> {
        let mut previous = current.checked_sub(1)?;
        while self
            .atoms
            .get(previous)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            previous = previous.checked_sub(1)?;
        }
        let class = self.atoms.get(previous)?.class;
        while previous > 0
            && self
                .atoms
                .get(previous - 1)
                .is_some_and(|atom| atom.class == class)
        {
            previous -= 1;
        }
        self.previous_point(previous)
    }

    fn next_word_end(&self, current: usize) -> Option<TerminalTextPoint<AbsRow>> {
        let mut next = current.saturating_add(1);
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        let class = self.atoms.get(next)?.class;
        while self
            .atoms
            .get(next + 1)
            .is_some_and(|atom| atom.class == class)
        {
            next += 1;
        }
        self.previous_point(next)
    }

    fn next_big_word_start(&self, current: usize) -> Option<TerminalTextPoint<AbsRow>> {
        let mut next = current.saturating_add(1);
        if self
            .atoms
            .get(current)
            .is_some_and(|atom| atom.class != TextClass::Whitespace)
        {
            while self
                .atoms
                .get(next)
                .is_some_and(|atom| atom.class != TextClass::Whitespace)
            {
                next += 1;
            }
        }
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        self.next_point(next)
    }

    fn previous_big_word_start(&self, current: usize) -> Option<TerminalTextPoint<AbsRow>> {
        let mut previous = current.checked_sub(1)?;
        while self
            .atoms
            .get(previous)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            previous = previous.checked_sub(1)?;
        }
        while previous > 0
            && self
                .atoms
                .get(previous - 1)
                .is_some_and(|atom| atom.class != TextClass::Whitespace)
        {
            previous -= 1;
        }
        self.previous_point(previous)
    }

    fn next_big_word_end(&self, current: usize) -> Option<TerminalTextPoint<AbsRow>> {
        let mut next = current.saturating_add(1);
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        self.atoms.get(next)?;
        while self
            .atoms
            .get(next + 1)
            .is_some_and(|atom| atom.class != TextClass::Whitespace)
        {
            next += 1;
        }
        self.previous_point(next)
    }

    fn next_point(&self, mut index: usize) -> Option<TerminalTextPoint<AbsRow>> {
        while let Some(atom) = self.atoms.get(index) {
            if let Some(point) = atom.point {
                return Some(point);
            }
            index += 1;
        }
        None
    }

    fn previous_point(&self, mut index: usize) -> Option<TerminalTextPoint<AbsRow>> {
        loop {
            if let Some(point) = self.atoms.get(index)?.point {
                return Some(point);
            }
            index = index.checked_sub(1)?;
        }
    }

    fn point_is_final_atom(&self, point: TerminalTextPoint<AbsRow>) -> bool {
        // Word motion targets are atom start points, so compare against the
        // final atom's start point. Comparing against `end_col` would never
        // match a wide glyph, whose end column is one past its start.
        self.atoms
            .iter()
            .rev()
            .find(|atom| atom.point.is_some())
            .is_some_and(|atom| atom.point == Some(point))
    }
}

#[cfg(test)]
fn terminal_cell_text(graphemes: &[u32]) -> String {
    if graphemes.is_empty()
        || graphemes.first().copied() == Some(crate::ghostty::KITTY_UNICODE_PLACEHOLDER)
    {
        return " ".to_string();
    }
    graphemes
        .iter()
        .map(|codepoint| char::from_u32(*codepoint).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

fn text_class(text: &str) -> TextClass {
    let Some(ch) = text.chars().next() else {
        return TextClass::Whitespace;
    };
    if ch.is_whitespace() {
        TextClass::Whitespace
    } else if ch.is_ascii() && COPY_MODE_WORD_SEPARATORS.contains(ch) {
        TextClass::Separator
    } else {
        TextClass::Word
    }
}

fn text_fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn absolute_point(
    point: TerminalTextPoint<ScreenRow>,
    origin: AbsRow,
) -> TerminalTextPoint<AbsRow> {
    TerminalTextPoint {
        row: point.row.absolute(origin),
        col: point.col,
    }
}

fn screen_point(point: TerminalTextPoint<AbsRow>, origin: AbsRow) -> TerminalTextPoint<ScreenRow> {
    TerminalTextPoint {
        row: point
            .row
            .screen_row(origin)
            .unwrap_or(ScreenRow(usize::MAX)),
        col: point.col,
    }
}

/// One copy-mode search: logical lines are fed in reading order and only the
/// matches that can end up in the returned window are kept, so memory stays
/// bounded by the window size however long the history is.
struct TextSearch {
    regex: regex::Regex,
    window: MatchWindow,
}

impl TextSearch {
    fn new(
        query: &str,
        case_sensitive: bool,
        direction: TerminalSearchDirection,
        cursor: TerminalTextPoint<AbsRow>,
        previous: Option<(TerminalTextPoint<AbsRow>, TerminalTextPoint<AbsRow>)>,
        limit: usize,
    ) -> Option<Self> {
        if query.is_empty() || limit == 0 {
            return None;
        }
        let regex = regex::RegexBuilder::new(&regex::escape(query))
            .case_insensitive(!case_sensitive)
            .build()
            .ok()?;
        let origin = match direction {
            TerminalSearchDirection::Forward => previous.map_or(cursor, |(_, end)| end),
            TerminalSearchDirection::Backward => previous.map_or(cursor, |(start, _)| start),
        };
        Some(Self {
            regex,
            window: MatchWindow {
                direction,
                origin,
                limit,
                total: 0,
                target: None,
                first: Vec::new(),
                recent: VecDeque::new(),
                boundary: None,
                after: Vec::new(),
            },
        })
    }

    fn scan_line(
        &mut self,
        line: &LogicalTextLine,
        cols: u16,
        screen: crate::ghostty::ActiveScreen,
    ) {
        for found in self.regex.find_iter(&line.text) {
            // Only matches that start and end on cell boundaries count: a
            // query for a lone combining mark must not match inside a cell.
            let Ok(start) = line
                .spans
                .binary_search_by_key(&found.start(), |span| span.byte_start)
            else {
                continue;
            };
            let Ok(end) = line
                .spans
                .binary_search_by_key(&found.end(), |span| span.byte_end)
            else {
                continue;
            };
            self.window.push(TerminalTextMatch {
                start: line.spans[start].start,
                end: line.spans[end].end,
                source_fingerprint: text_fingerprint(found.as_str()),
                scan_cols: cols,
                scan_screen: screen,
            });
        }
    }

    fn finish(self) -> TerminalSearchWindow<AbsRow> {
        self.window.finish()
    }
}

/// The part of a search's match list that can end up in its window.
///
/// The target is the first match after the origin (forward) or the last one
/// before it (backward); without one, a forward search wraps to the first
/// match and a backward one to the last. The window is `limit` matches
/// around the target, so it never reaches more than `limit` matches either
/// side of it. Matches arrive in reading order, so the target is known as
/// soon as the scan passes the origin: until then the last `limit` matches
/// are kept, from the target on the next `limit`, and the first `limit`
/// always (for a forward wrap).
struct MatchWindow {
    direction: TerminalSearchDirection,
    origin: TerminalTextPoint<AbsRow>,
    limit: usize,
    total: usize,
    target: Option<usize>,
    first: Vec<TerminalTextMatch<AbsRow>>,
    /// The last `limit` matches before `boundary` (before the end while no
    /// boundary is set).
    recent: VecDeque<TerminalTextMatch<AbsRow>>,
    /// Index of the first match kept in `after`, once the target is known.
    boundary: Option<usize>,
    after: Vec<TerminalTextMatch<AbsRow>>,
}

impl MatchWindow {
    fn push(&mut self, text_match: TerminalTextMatch<AbsRow>) {
        let index = self.total;
        self.total = self.total.saturating_add(1);
        if self.first.len() < self.limit {
            self.first.push(text_match);
        }
        if self.boundary.is_none() {
            match self.direction {
                TerminalSearchDirection::Forward => {
                    if text_match.start > self.origin {
                        self.target = Some(index);
                        self.boundary = Some(index);
                    }
                }
                TerminalSearchDirection::Backward => {
                    if text_match.end < self.origin {
                        self.target = Some(index);
                    } else if self.target.is_some() {
                        // Match ends only grow, so no later match can be the
                        // target. Without a target the search wraps to the
                        // last match, which the recent matches keep tracking.
                        self.boundary = Some(index);
                    }
                }
            }
        }
        if self.boundary.is_some() {
            if self.after.len() < self.limit {
                self.after.push(text_match);
            }
        } else {
            if self.recent.len() == self.limit {
                self.recent.pop_front();
            }
            self.recent.push_back(text_match);
        }
    }

    fn get(&self, index: usize) -> Option<TerminalTextMatch<AbsRow>> {
        if let Some(text_match) = self.first.get(index) {
            return Some(*text_match);
        }
        if let Some(boundary) = self.boundary
            && index >= boundary
        {
            return self.after.get(index - boundary).copied();
        }
        let recent_start = self
            .boundary
            .unwrap_or(self.total)
            .saturating_sub(self.recent.len());
        self.recent.get(index.checked_sub(recent_start)?).copied()
    }

    fn finish(self) -> TerminalSearchWindow<AbsRow> {
        let total = self.total;
        if total == 0 {
            return TerminalSearchWindow::empty();
        }
        let target = self.target.unwrap_or(match self.direction {
            TerminalSearchDirection::Forward => 0,
            TerminalSearchDirection::Backward => total - 1,
        });
        let retained = self.limit.min(total);
        let start = target
            .saturating_sub(retained / 2)
            .min(total.saturating_sub(retained));
        let end = start.saturating_add(retained);
        TerminalSearchWindow {
            matches: (start..end).filter_map(|index| self.get(index)).collect(),
            current: Some(target - start),
            current_global: Some(target),
            total,
        }
    }
}

/// Word motion on the live terminal, with absolute rows. Reads a window of
/// rows around the start and widens it while the answer may lie past its
/// edge (a word continuing across a soft wrap at the window's edge).
fn word_motion_in(
    terminal: &crate::ghostty::Terminal,
    point: TerminalTextPoint<AbsRow>,
    motion: TerminalWordMotion,
) -> Option<TerminalTextPoint<AbsRow>> {
    let total_rows = terminal.total_rows();
    let row = terminal.screen_row_for_absolute(point.row)?.0;
    let backward = matches!(
        motion,
        TerminalWordMotion::PreviousStart | TerminalWordMotion::PreviousBigStart
    );
    let to_word_end = matches!(
        motion,
        TerminalWordMotion::NextEnd | TerminalWordMotion::NextBigEnd
    );
    let mut window_rows = 64usize;
    loop {
        let (start_row, end_row) = if backward {
            (row.saturating_sub(window_rows.saturating_sub(1)), row + 1)
        } else {
            (row, row.saturating_add(window_rows).min(total_rows))
        };
        let (buffer, first, last) = RetainedTextBuffer::live_words(terminal, start_row, end_row)?;
        let starts_in_continuation = first.wrap_continuation && start_row > 0;
        let ends_in_continuation = last.soft_wrapped && end_row < total_rows;
        let target = buffer.word_motion(point.row, point.col, motion);
        let needs_more_history = backward
            && starts_in_continuation
            && target.is_some_and(|target| {
                target.row == terminal.absolute_row_for_screen(ScreenRow(start_row))
            });
        let needs_more_future = to_word_end
            && ends_in_continuation
            && target.is_some_and(|target| buffer.point_is_final_atom(target));
        if target.is_some() && !needs_more_history && !needs_more_future {
            return target;
        }
        let reached_edge = if backward {
            start_row == 0
        } else {
            end_row == total_rows
        };
        if reached_edge {
            return target;
        }
        window_rows = window_rows.saturating_mul(2).min(total_rows);
    }
}

/// The next blank row above (`direction < 0`) or below absolute row `row`,
/// looking at most 1000 rows away.
fn paragraph_motion_in(
    terminal: &crate::ghostty::Terminal,
    row: AbsRow,
    direction: i8,
) -> Option<TerminalTextPoint<AbsRow>> {
    let total_rows = terminal.total_rows();
    let current = terminal.screen_row_for_absolute(row)?.0;
    if direction == 0 {
        return None;
    }
    let mut scratch = String::new();
    for distance in 1..total_rows.min(1000) {
        let candidate = if direction < 0 {
            current.checked_sub(distance)?
        } else {
            let candidate = current.saturating_add(distance);
            if candidate >= total_rows {
                return None;
            }
            candidate
        };
        let mut blank = true;
        terminal.visit_screen_row_text(ScreenRow(candidate), &mut scratch, |_, _, text| {
            blank &= text.chars().all(char::is_whitespace);
        })?;
        if blank {
            return Some(TerminalTextPoint {
                row: terminal.absolute_row_for_screen(ScreenRow(candidate)),
                col: 0,
            });
        }
    }
    None
}

impl GhosttyPaneTerminal {
    pub fn new(mut terminal: crate::ghostty::Terminal) -> Self {
        // Replies to anything written before the pane existed have no reader.
        let _ = terminal.take_pty_responses();

        let mut render_state = crate::ghostty::RenderState::new();
        render_state.update(&terminal);
        let initial_colors = render_state.colors();
        let initial_default_foreground = Some(initial_colors.foreground);
        let initial_default_background = Some(initial_colors.background);
        Self {
            core: Mutex::new(GhosttyPaneCore {
                #[cfg(test)]
                dirty_collection_hook: None,
                terminal,
                synchronized_output_epoch: 0,
                render_state,
                initial_default_foreground,
                initial_default_background,
                host_terminal_theme: crate::host_term::theme::TerminalTheme::default(),
                transient_default_color_owner_pgid: None,
                default_color_generation: 0,
                osc_debug_tracker: OscDebugTracker::default(),
                agent_osc_state: AgentOscStateTracker::default(),
            }),
        }
    }

    /// Installs the host theme as the pane's default palette and default
    /// colours. They sit under whatever the child set itself (OSC 4/10/11),
    /// which stays in effect; nothing is written into the child's stream.
    pub fn apply_host_terminal_theme(&self, theme: crate::host_term::theme::TerminalTheme) {
        if let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) {
            core.host_terminal_theme = theme;
            if !has_default_color_override(&core.terminal) {
                core.transient_default_color_owner_pgid = None;
            }

            let mut palette = crate::ghostty::default_palette();
            for (index, color) in theme.palette.iter().enumerate() {
                if let Some(color) = color {
                    palette[index] = *color;
                }
            }
            core.terminal.set_default_palette(&palette);
            core.terminal
                .set_default_colors(theme.foreground, theme.background);
        }
    }

    pub fn apply_host_terminal_appearance(
        &self,
        appearance: Option<crate::host_term::theme::HostAppearance>,
    ) -> Option<Bytes> {
        let mut core = crate::ghostty::lock_terminal_core(&self.core).ok()?;
        let color_scheme = appearance;
        let previous = core.terminal.set_color_scheme(color_scheme);

        let transitioned = matches!(
            (previous, color_scheme),
            (Some(previous), Some(current)) if previous != current
        );
        if !transitioned
            || !core
                .terminal
                .mode_get(crate::ghostty::MODE_COLOR_SCHEME_REPORT)
        {
            return None;
        }
        appearance.map(|appearance| Bytes::from_static(appearance.report()))
    }

    pub fn has_transient_default_color_override(&self) -> bool {
        crate::ghostty::lock_terminal_core(&self.core)
            .map(|core| core.transient_default_color_owner_pgid.is_some())
            .unwrap_or(false)
    }

    pub fn maybe_restore_host_terminal_theme(&self, pane_id: PaneId, shell_pid: u32) -> bool {
        {
            let Ok(core) = crate::ghostty::lock_terminal_core(&self.core) else {
                return false;
            };
            if !should_probe_host_terminal_theme_restore(&core) {
                return false;
            }
        }

        let foreground_job = crate::detect::foreground_job(shell_pid);
        let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return false;
        };

        let alternate_screen =
            core.terminal.active_screen() == crate::ghostty::ActiveScreen::Alternate;
        restore_host_terminal_theme_if_needed(
            &mut core,
            pane_id,
            shell_pid,
            alternate_screen,
            foreground_job.as_ref(),
        )
    }

    pub fn terminal_title(&self) -> Option<String> {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|core| core.agent_osc_state.terminal_title().map(str::to_string))
    }

    /// Returns the latest OSC 0/2 title retained for agent detection, or `""`
    /// if no title has been seen or the last update was an empty clear.
    pub fn agent_osc_title(&self) -> String {
        crate::ghostty::lock_terminal_core(&self.core)
            .map(|core| core.agent_osc_state.latest_title().to_owned())
            .unwrap_or_default()
    }

    /// Returns the latest OSC 9 progress payload retained for agent detection,
    /// or `""` if none has been seen.
    pub fn agent_osc_progress(&self) -> String {
        crate::ghostty::lock_terminal_core(&self.core)
            .map(|core| core.agent_osc_state.latest_progress().to_owned())
            .unwrap_or_default()
    }

    /// Clears retained OSC title/progress evidence when the pane's foreground
    /// agent changes, so a new agent process starts from a blank OSC slate.
    pub fn clear_agent_osc_state(&self) {
        if let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) {
            core.agent_osc_state.clear_retained();
        }
    }

    pub fn process_pty_bytes(
        &self,
        pane_id: PaneId,
        _shell_pid: u32,
        bytes: &[u8],
    ) -> ProcessBytesResult {
        let mut core = match crate::ghostty::lock_terminal_core(&self.core) {
            Ok(core) => core,
            Err(crate::ghostty::TerminalCorePoisoned) => {
                // The core may be inconsistent after a panic. Fail the pane
                // so its reader stops and the pane is reported dead.
                error!(pane = pane_id.raw(), "ghostty core lock poisoned in reader");
                return ProcessBytesResult {
                    core_poisoned: true,
                    ..ProcessBytesResult::default()
                };
            }
        };

        core.osc_debug_tracker.observe(bytes);
        for event in core.osc_debug_tracker.drain_pending() {
            debug!(
                pane = pane_id.raw(),
                osc_command = %event.command,
                osc_payload = ?event.payload,
                "agent OSC evidence observed"
            );
        }

        let synchronized_output_before = core
            .terminal
            .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT);
        core.terminal.write(bytes);
        // Everything the core queued is collected here, including the effects
        // of a timed-out synchronized update that a render flushed since the
        // last read: those are late, but dropping them would be worse.
        let effects = collect_core_effects(&mut core);
        let default_color_generation = core.default_color_generation;

        let synchronized_output = core
            .terminal
            .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT);
        if synchronized_output != synchronized_output_before {
            core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
        }
        let request_render = !synchronized_output;
        // A synchronized update that never ends is force-flushed by the core
        // after its timeout; schedule a render for then so the pane does not
        // stay frozen until the next PTY read.
        let render_delay = if synchronized_output {
            core.terminal
                .synchronized_output_deadline()
                .map(|deadline| {
                    deadline.saturating_duration_since(Instant::now())
                        + SYNCHRONIZED_OUTPUT_FLUSH_MARGIN
                })
        } else {
            None
        };
        drop(core);
        ProcessBytesResult {
            request_render,
            render_delay,
            terminal_title_changed: effects.terminal_title_changed,
            clipboard_writes: effects.clipboard_writes,
            reported_cwd: effects.reported_cwd,
            terminal_responses: effects.terminal_responses,
            default_color_owner_pending: effects.default_color_owner_pending,
            default_color_generation,
            core_poisoned: false,
        }
    }

    /// Records which foreground program overrode a default colour, so the
    /// detection tick can drop the override once that program is gone.
    ///
    /// Finding the program means scanning `/proc`. The caller releases the
    /// terminal and content locks before this scan, then this method takes
    /// the terminal lock briefly to store the answer. The generation check
    /// drops an answer if another OSC colour write arrived during the scan.
    fn resolve_default_color_owner(&self, pane_id: PaneId, shell_pid: u32, generation: u64) {
        if shell_pid == 0 {
            return;
        }
        let Some(owner_pgid) = current_transient_default_color_owner(shell_pid) else {
            return;
        };
        let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return;
        };
        if core.default_color_generation == generation && has_default_color_override(&core.terminal)
        {
            core.transient_default_color_owner_pgid = Some(owner_pgid);
            debug!(
                pane = pane_id.raw(),
                owner_pgid, "tracked transient default color override"
            );
        }
    }

    /// Force-ends a synchronized update whose timeout has passed and returns
    /// everything the core has queued for delivery: replies for the child,
    /// OSC 52 writes, a working-directory report, a title change. Meant for
    /// the timer the reader arms from [`ProcessBytesResult::render_delay`]:
    /// a child that sent a query inside a frame it never ended waits for the
    /// reply, and nothing else would deliver it before its next output.
    /// `request_render` is set when a frame was flushed.
    pub(crate) fn flush_expired_synchronized_output(
        &self,
        _pane_id: PaneId,
        _shell_pid: u32,
    ) -> ProcessBytesResult {
        let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) else {
            // A poisoned core is noticed by the PTY actor (its per-loop
            // `core_poisoned` check, or its next read), which ends the pane;
            // this timer has no loop to stop.
            return ProcessBytesResult {
                core_poisoned: true,
                ..ProcessBytesResult::default()
            };
        };
        let flushed = core.terminal.flush_expired_synchronized_output();
        if flushed {
            core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
        }
        let effects = collect_core_effects(&mut core);
        let default_color_generation = core.default_color_generation;
        drop(core);
        ProcessBytesResult {
            request_render: flushed,
            render_delay: None,
            terminal_title_changed: effects.terminal_title_changed,
            clipboard_writes: effects.clipboard_writes,
            reported_cwd: effects.reported_cwd,
            terminal_responses: effects.terminal_responses,
            default_color_owner_pending: effects.default_color_owner_pending,
            default_color_generation,
            core_poisoned: false,
        }
    }

    pub fn seed_history_ansi(&self, ansi: &str) {
        if ansi.is_empty() {
            return;
        }
        let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return;
        };
        core.terminal.write(ansi.as_bytes());
        // Saved history is trimmed, so it normally ends on the last restored
        // line with no line break. Without one the cursor stays at the end of
        // that line and the fresh shell prints its first prompt glued onto it.
        if !ansi.ends_with('\n') {
            core.terminal.write(b"\r\n");
        }
        // Restored history must never answer the live child, nor surface as
        // live clipboard writes, directory reports or title and colour
        // changes.
        discard_core_effects(&mut core.terminal);
    }

    pub fn resize(&self, geometry: crate::geometry::PaneGeometry) -> Vec<Bytes> {
        let rows = geometry.rows();
        if let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) {
            let synchronized_output_before = core
                .terminal
                .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT);
            let offset_from_bottom = core.terminal.scrollbar();
            let offset_from_bottom = offset_from_bottom
                .total
                .saturating_sub(offset_from_bottom.offset + offset_from_bottom.len);
            let resize_recovery_probe_lines = usize::from(rows)
                .saturating_mul(8)
                .max(DEFAULT_DETECTION_ROWS);

            // Replies already queued (a render may have flushed a timed-out
            // synchronized update) stay queued for the next read: the
            // resize's own replies go to a slot the next resize overwrites.
            let pending_responses = core.terminal.take_pty_responses();
            // No history is replayed into the core after the resize. That was
            // a workaround for the libghostty core losing rows on resize;
            // alacritty reflows bottom-anchored and keeps the rows above the
            // cursor (a shrink drops only rows below it, as Terminal.app and
            // iTerm do), and a replay fed bytes through the child's parser,
            // cutting into any sequence it had half-written and moving its
            // cursor behind its back.
            core.terminal.resize(geometry);
            let synchronized_output_after = core
                .terminal
                .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT);
            if synchronized_output_after != synchronized_output_before {
                core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
            }
            let terminal_responses = drain_terminal_responses(&mut core);
            core.terminal.restore_pty_responses(pending_responses);

            ghostty_set_scroll_offset_from_bottom(&mut core.terminal, offset_from_bottom);
            if offset_from_bottom > 0 {
                let mut remaining = offset_from_bottom.min(resize_recovery_probe_lines);
                while remaining > 0 && ghostty_visible_text(&mut core).trim().is_empty() {
                    core.terminal.scroll_viewport_delta(1);
                    remaining -= 1;
                }
            }
            terminal_responses
        } else {
            Vec::new()
        }
    }

    pub fn scroll_up(&self, lines: usize) {
        if let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) {
            let lines = isize::try_from(lines).unwrap_or(isize::MAX);
            core.terminal.scroll_viewport_delta(-lines);
        }
    }

    pub fn scroll_down(&self, lines: usize) {
        if let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) {
            let lines = isize::try_from(lines).unwrap_or(isize::MAX);
            core.terminal.scroll_viewport_delta(lines);
        }
    }

    pub fn scroll_reset(&self) {
        if let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) {
            core.terminal.scroll_viewport_bottom();
        }
    }

    pub fn clear_screen(&self) -> Result<(), PaneClearError> {
        let mut core = crate::ghostty::lock_terminal_core(&self.core)
            .map_err(|_| PaneClearError::TerminalLockPoisoned)?;
        let _ = core.terminal.clear_screen();
        Ok(())
    }

    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        if let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) {
            ghostty_set_scroll_offset_from_bottom(&mut core.terminal, lines);
        }
    }

    pub fn scroll_metrics(&self) -> Option<ScrollMetrics> {
        let Ok(core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return None;
        };
        Some(terminal_scroll_metrics(&core.terminal))
    }

    pub fn scroll_position(&self) -> Option<ScrollPosition> {
        let core = crate::ghostty::lock_terminal_core(&self.core).ok()?;
        Some(ScrollPosition {
            metrics: terminal_scroll_metrics(&core.terminal),
        })
    }

    pub(crate) fn history_origin(&self) -> Option<AbsRow> {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .map(|core| core.terminal.history_origin())
    }

    /// Chunked copy-mode search with absolute rows; see
    /// [`PaneTerminal::search_text_window_absolute`].
    pub(crate) fn search_text_window(
        &self,
        query: &str,
        case_sensitive: bool,
        direction: TerminalSearchDirection,
        cursor: TerminalTextPoint<AbsRow>,
        previous: Option<(TerminalTextPoint<AbsRow>, TerminalTextPoint<AbsRow>)>,
        limit: usize,
    ) -> TerminalSearchWindow<AbsRow> {
        let Some(mut search) =
            TextSearch::new(query, case_sensitive, direction, cursor, previous, limit)
        else {
            return TerminalSearchWindow::empty();
        };
        let mut builder = TextBufferBuilder::new(true, false);
        let mut scratch = String::new();
        let mut next = None;
        let mut scan = None;
        loop {
            let Ok(core) = crate::ghostty::lock_terminal_core(&self.core) else {
                break;
            };
            let terminal = &core.terminal;
            let cols = terminal.cols();
            let screen = terminal.active_screen();
            let total_rows = terminal.total_rows();
            let (scan_cols, scan_screen) = *scan.get_or_insert((cols, screen));
            if (scan_cols, scan_screen) != (cols, screen) {
                // Re-wrapped or switched screens while the lock was
                // released: the rest would not continue the same text.
                break;
            }
            let origin = terminal.history_origin();
            let end = terminal.absolute_row_for_screen(ScreenRow(total_rows));
            let mut row = next.unwrap_or(origin);
            if row < origin {
                // Lines were evicted while the lock was released, possibly
                // the start of the line being assembled.
                builder.discard_line();
                row = origin;
            }
            let chunk_end = end.min(row.saturating_add(SCAN_CHUNK_ROWS));
            while row < chunk_end {
                let Some(y) = terminal.screen_row_for_absolute(row) else {
                    break;
                };
                let Some(wrap) =
                    terminal.visit_screen_row_text(y, &mut scratch, |col, wide, text| {
                        builder.push_cell(row, col, wide, text);
                    })
                else {
                    break;
                };
                if builder.end_row(wrap.soft_wrapped) {
                    search.scan_line(&builder.line, cols, screen);
                }
                row = row.saturating_add(1);
            }
            drop(core);
            if row < chunk_end || row >= end {
                break;
            }
            next = Some(row);
            // Give the PTY reader waiting on the lock a chance to take it.
            std::thread::yield_now();
        }
        if let (Some(line), Some((cols, screen))) = (builder.trailing_line(), scan) {
            search.scan_line(line, cols, screen);
        }
        search.finish()
    }

    pub fn keyboard_protocol(&self) -> Option<crate::input::KeyboardProtocol> {
        let Ok(core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return None;
        };
        Some(crate::input::KeyboardProtocol::from_kitty_flags(
            core.terminal.kitty_keyboard_flags().bits(),
        ))
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.mode_enabled(crate::ghostty::MODE_BRACKETED_PASTE)
    }

    pub fn focus_reporting_enabled(&self) -> bool {
        self.mode_enabled(crate::ghostty::MODE_FOCUS_EVENT)
    }

    pub fn mouse_reporting_enabled(&self) -> bool {
        crate::ghostty::lock_terminal_core(&self.core)
            .is_ok_and(|core| core.terminal.mouse_tracking_enabled())
    }

    pub fn modify_other_keys_level(&self) -> u8 {
        crate::ghostty::lock_terminal_core(&self.core)
            .map_or(0, |core| core.terminal.modify_other_keys_level().as_u8())
    }

    pub fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.mode_enabled(crate::ghostty::MODE_MOUSE_SGR_PIXELS)
    }

    fn mode_enabled(&self, mode: u16) -> bool {
        crate::ghostty::lock_terminal_core(&self.core)
            .is_ok_and(|core| core.terminal.mode_get(mode))
    }

    pub fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        let core = crate::ghostty::lock_terminal_core(&self.core).ok()?;
        let alternate_screen =
            core.terminal.active_screen() == crate::ghostty::ActiveScreen::Alternate;
        let mouse_reporting = core.terminal.mouse_tracking_enabled();
        let application_cursor = core
            .terminal
            .mode_get(crate::ghostty::MODE_APPLICATION_CURSOR_KEYS);
        let bracketed_paste = core.terminal.mode_get(crate::ghostty::MODE_BRACKETED_PASTE);
        Some(!alternate_screen && !mouse_reporting && (!application_cursor || bracketed_paste))
    }

    pub fn alternate_screen_active(&self) -> bool {
        crate::ghostty::lock_terminal_core(&self.core).is_ok_and(|core| {
            core.terminal.active_screen() == crate::ghostty::ActiveScreen::Alternate
        })
    }

    // This aggregate snapshot performs multiple terminal queries. Pane-scaled
    // callers should add a narrow accessor instead.
    #[cfg(test)]
    pub fn input_state(&self) -> Option<InputState> {
        let Ok(core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return None;
        };
        let alternate_screen =
            core.terminal.active_screen() == crate::ghostty::ActiveScreen::Alternate;
        let application_cursor = core
            .terminal
            .mode_get(crate::ghostty::MODE_APPLICATION_CURSOR_KEYS);
        let bracketed_paste = core.terminal.mode_get(crate::ghostty::MODE_BRACKETED_PASTE);
        let focus_reporting = core.terminal.mode_get(crate::ghostty::MODE_FOCUS_EVENT);
        let mouse_sgr = core.terminal.mode_get(crate::ghostty::MODE_MOUSE_SGR);
        let mouse_utf8 = core.terminal.mode_get(crate::ghostty::MODE_MOUSE_UTF8);
        let mouse_sgr_pixels = core
            .terminal
            .mode_get(crate::ghostty::MODE_MOUSE_SGR_PIXELS);
        let mouse_alternate_scroll = core
            .terminal
            .mode_get(crate::ghostty::MODE_MOUSE_ALTERNATE_SCROLL);
        let mouse_protocol_mode = if core.terminal.mode_get(MODE_MOUSE_ANY_MOTION) {
            crate::input::MouseProtocolMode::AnyMotion
        } else if core.terminal.mode_get(MODE_MOUSE_BUTTON_MOTION) {
            crate::input::MouseProtocolMode::ButtonMotion
        } else if core.terminal.mode_get(MODE_MOUSE_PRESS_RELEASE) {
            crate::input::MouseProtocolMode::PressRelease
        } else if core.terminal.mode_get(MODE_MOUSE_X10) {
            crate::input::MouseProtocolMode::Press
        } else {
            crate::input::MouseProtocolMode::None
        };
        let mouse_protocol_encoding = if mouse_sgr_pixels {
            crate::input::MouseProtocolEncoding::SgrPixels
        } else if mouse_sgr {
            crate::input::MouseProtocolEncoding::Sgr
        } else if mouse_utf8 {
            crate::input::MouseProtocolEncoding::Utf8
        } else {
            crate::input::MouseProtocolEncoding::Default
        };
        Some(InputState {
            alternate_screen,
            application_cursor,
            bracketed_paste,
            focus_reporting,
            mouse_protocol_mode,
            mouse_protocol_encoding,
            mouse_alternate_scroll,
            modify_other_keys: core.terminal.modify_other_keys_level()
                == crate::ghostty::ModifyOtherKeysLevel::All,
            color_scheme_reporting: core
                .terminal
                .mode_get(crate::ghostty::MODE_COLOR_SCHEME_REPORT),
        })
    }

    pub fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        let Ok(core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return None;
        };
        let alternate_screen =
            core.terminal.active_screen() == crate::ghostty::ActiveScreen::Alternate;
        let mouse_alternate_scroll = core
            .terminal
            .mode_get(crate::ghostty::MODE_MOUSE_ALTERNATE_SCROLL);
        let mouse_reporting = core.terminal.mode_get(MODE_MOUSE_ANY_MOTION)
            || core.terminal.mode_get(MODE_MOUSE_BUTTON_MOTION)
            || core.terminal.mode_get(MODE_MOUSE_PRESS_RELEASE)
            || core.terminal.mode_get(MODE_MOUSE_X10);
        Some(if mouse_reporting {
            crate::pane::WheelRouting::MouseReport
        } else if alternate_screen && mouse_alternate_scroll {
            crate::pane::WheelRouting::AlternateScroll
        } else {
            crate::pane::WheelRouting::HostScroll
        })
    }

    pub fn cursor_state(&self) -> Option<TerminalCursorState> {
        let mut core = crate::ghostty::lock_terminal_core(&self.core).ok()?;
        current_cursor_state(&mut core)
    }

    pub fn synchronized_output_active(&self) -> bool {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .is_some_and(|mut core| {
                flush_expired_synchronized_output(&mut core);
                core.terminal
                    .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT)
            })
    }

    pub(crate) fn synchronized_output_state(&self) -> (bool, u64) {
        crate::ghostty::lock_terminal_core(&self.core)
            .map(|mut core| {
                flush_expired_synchronized_output(&mut core);
                (
                    core.terminal
                        .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT),
                    core.synchronized_output_epoch,
                )
            })
            .unwrap_or((true, 0))
    }

    pub fn encode_terminal_key(
        &self,
        key: crate::input::TerminalKey,
        protocol: crate::input::KeyboardProtocol,
    ) -> Vec<u8> {
        let repeat_count = key.repeat_count;
        let first = key.with_repeat_count(1);
        let mut bytes = self.encode_terminal_key_once(first.clone(), protocol);
        if repeat_count > 1 && first.kind != crossterm::event::KeyEventKind::Release {
            let repeated = first.with_kind(crossterm::event::KeyEventKind::Repeat);
            let repeated_bytes = self.encode_terminal_key_once(repeated, protocol);
            for _ in 1..repeat_count {
                bytes.extend_from_slice(&repeated_bytes);
            }
        }
        bytes
    }

    fn encode_terminal_key_once(
        &self,
        key: crate::input::TerminalKey,
        protocol: crate::input::KeyboardProtocol,
    ) -> Vec<u8> {
        // Character keys follow the caller's protocol; every other key follows
        // the modes the child negotiated with this pane.
        if matches!(key.code, crossterm::event::KeyCode::Char(_)) {
            return crate::input::encode_terminal_key(key, protocol);
        }
        let Some(modes) = crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .map(|core| crate::input::KeyEncodeModes {
                kitty_flags: core.terminal.kitty_keyboard_flags().bits(),
                modify_other_keys: core.terminal.modify_other_keys_level().as_u8(),
                application_cursor: core
                    .terminal
                    .mode_get(crate::ghostty::MODE_APPLICATION_CURSOR_KEYS),
            })
        else {
            return crate::input::encode_terminal_key(key, protocol);
        };
        crate::input::encode_terminal_key_with_modes(key, modes)
    }

    pub(crate) fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        use crossterm::event::MouseEventKind;
        if !matches!(
            kind,
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_)
        ) {
            return None;
        }
        self.encode_mouse_event(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if kind != crossterm::event::MouseEventKind::Moved {
            return None;
        }
        self.encode_mouse_event(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        use crossterm::event::MouseEventKind;
        if !matches!(
            kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            return None;
        }
        self.encode_mouse_event(kind, position, modifiers)
    }

    fn encode_mouse_event(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        let core = crate::ghostty::lock_terminal_core(&self.core).ok()?;
        let terminal = &core.terminal;
        let mode_enabled = |mode: u16| terminal.mode_get(mode);
        let mode = if mode_enabled(MODE_MOUSE_ANY_MOTION) {
            crate::input::MouseProtocolMode::AnyMotion
        } else if mode_enabled(MODE_MOUSE_BUTTON_MOTION) {
            crate::input::MouseProtocolMode::ButtonMotion
        } else if mode_enabled(MODE_MOUSE_PRESS_RELEASE) {
            crate::input::MouseProtocolMode::PressRelease
        } else if mode_enabled(MODE_MOUSE_X10) {
            crate::input::MouseProtocolMode::Press
        } else {
            return None;
        };
        let cell_encoding = if mode_enabled(crate::ghostty::MODE_MOUSE_SGR) {
            crate::input::MouseProtocolEncoding::Sgr
        } else if mode_enabled(crate::ghostty::MODE_MOUSE_UTF8) {
            crate::input::MouseProtocolEncoding::Utf8
        } else {
            crate::input::MouseProtocolEncoding::Default
        };
        let sgr_pixels = mode_enabled(crate::ghostty::MODE_MOUSE_SGR_PIXELS);
        // Reports are 1-based. Pixel positions already arrive 1-based; cell
        // positions are shifted here. Under SGR-pixels (mode 1016) a cell
        // position is mapped to the top-left pixel of that cell using the same
        // integer cell pitch the pixel fallback below uses, so the child maps
        // it straight back to the cell. Only when the pane has no pixel
        // geometry at all is the cell sent as-is in SGR form: the child can't
        // know a cell size either then, and a report beats a dropped click.
        let cell_pitch = || {
            let cols = u32::from(terminal.cols());
            let rows = u32::from(terminal.rows());
            let width_px = terminal.width_px();
            let height_px = terminal.height_px();
            (cols > 0 && rows > 0 && width_px > 0 && height_px > 0)
                .then(|| ((width_px / cols).max(1), (height_px / rows).max(1)))
        };
        let (encoding, x, y) = match position {
            crate::input::mouse::Position::Cell { column, row } if sgr_pixels => {
                match cell_pitch() {
                    Some((cell_width, cell_height)) => (
                        crate::input::MouseProtocolEncoding::SgrPixels,
                        u32::from(column)
                            .saturating_mul(cell_width)
                            .saturating_add(1),
                        u32::from(row).saturating_mul(cell_height).saturating_add(1),
                    ),
                    None => (
                        crate::input::MouseProtocolEncoding::Sgr,
                        u32::from(column) + 1,
                        u32::from(row) + 1,
                    ),
                }
            }
            crate::input::mouse::Position::Cell { column, row } => {
                (cell_encoding, u32::from(column) + 1, u32::from(row) + 1)
            }
            crate::input::mouse::Position::Pixels { x, y } if sgr_pixels => {
                (crate::input::MouseProtocolEncoding::SgrPixels, x, y)
            }
            crate::input::mouse::Position::Pixels { x, y } => {
                let cols = u32::from(terminal.cols());
                let rows = u32::from(terminal.rows());
                let (cell_width, cell_height) = cell_pitch()?;
                (
                    cell_encoding,
                    (x.saturating_sub(1) / cell_width).min(cols - 1) + 1,
                    (y.saturating_sub(1) / cell_height).min(rows - 1) + 1,
                )
            }
        };
        crate::input::encode_mouse_event(kind, x, y, modifiers, mode, encoding)
    }

    /// The active screen, its width and, on the alternate screen only, its
    /// rows as owned text. Every caller (the alt-screen history read and its
    /// guards) falls back as soon as it sees the primary screen, where the
    /// retained rows are the whole scrollback: copying them cell by cell under
    /// the core lock only to be dropped is pure waste, so the rows come back
    /// empty there.
    pub(crate) fn screen_text_snapshot(
        &self,
    ) -> Option<(
        crate::ghostty::ActiveScreen,
        u16,
        Vec<crate::ghostty::ScreenTextRow>,
    )> {
        let core = crate::ghostty::lock_terminal_core(&self.core).ok()?;
        let screen = core.terminal.active_screen();
        let rows = match screen {
            crate::ghostty::ActiveScreen::Alternate => core.terminal.screen_text_rows(),
            crate::ghostty::ActiveScreen::Primary => Vec::new(),
        };
        Some((screen, core.terminal.cols(), rows))
    }

    pub fn visible_text(&self) -> String {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .map(|mut core| ghostty_visible_text(&mut core))
            .unwrap_or_default()
    }

    pub fn visible_ansi(&self) -> String {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|core| ghostty_visible_ansi(&core).ok())
            .unwrap_or_default()
    }

    pub fn detection_text(&self) -> String {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| ghostty_detection_text(&mut core).ok())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn recent_text(&self, lines: usize) -> String {
        self.recent_text_snapshot(lines).text
    }

    pub(crate) fn recent_text_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| ghostty_recent_text_snapshot(&mut core, lines).ok())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn recent_ansi(&self, lines: usize) -> String {
        self.recent_ansi_snapshot(lines).text
    }

    pub(crate) fn recent_ansi_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| ghostty_recent_ansi_snapshot(&mut core, lines, false).ok())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.recent_unwrapped_text_snapshot(lines).text
    }

    pub(crate) fn recent_unwrapped_text_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| ghostty_recent_text_unwrapped_snapshot(&mut core, lines).ok())
            .unwrap_or_default()
    }

    pub(crate) fn recent_unwrapped_ansi_snapshot(&self, lines: usize) -> TerminalReadSnapshot {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| ghostty_recent_ansi_snapshot(&mut core, lines, true).ok())
            .unwrap_or_default()
    }

    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| ghostty_extract_selection(&mut core, selection))
    }

    pub fn primary_history_ansi(&self) -> Option<String> {
        let mut core = crate::ghostty::lock_terminal_core(&self.core).ok()?;
        if core.terminal.active_screen() != crate::ghostty::ActiveScreen::Primary {
            return None;
        }
        ghostty_recent_ansi_snapshot(&mut core, usize::MAX, true)
            .ok()
            .map(|snapshot| snapshot.text)
    }

    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .and_then(|mut core| ghostty_visible_hyperlinks(&mut core, area).ok())
            .unwrap_or_default()
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        let Ok(mut core) = crate::ghostty::lock_terminal_core(&self.core) else {
            return;
        };
        flush_expired_synchronized_output(&mut core);
        if core
            .terminal
            .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT)
        {
            return;
        }
        let host_theme = core.host_terminal_theme;
        let initial_default_foreground = core.initial_default_foreground;
        let initial_default_background = core.initial_default_background;
        let GhosttyPaneCore {
            terminal,
            render_state,
            ..
        } = &mut *core;
        render_state.update(terminal);
        let cursor_shape_overridden = terminal.cursor_shape_overridden();
        let colors = render_state.colors();
        let default_bg =
            ghostty_default_bg(colors.background, host_theme, initial_default_background);
        let default_fg =
            ghostty_default_fg(colors.foreground, host_theme, initial_default_foreground);
        let resolved_fg = Some(ghostty_color(colors.foreground));
        let resolved_bg = Some(ghostty_color(colors.background));
        let default_palette = terminal.default_palette();
        let palette_overrides = PaletteOverrides::new(&colors.palette, &default_palette);
        // Shepr never renders kitty graphics, but a program may still emit the
        // unicode placeholder codepoint as literal text; always hide it so a
        // stray private-use glyph doesn't leak into the rendered pane.
        let hide_kitty_placeholders = true;

        {
            let buf = frame.buffer_mut();
            let mut symbol_scratch = String::new();
            let mut y = 0u16;
            for row in render_state.iter_rows().take(usize::from(area.height)) {
                let mut cells = row.cells().take(usize::from(area.width));
                let mut x = 0u16;
                for cell_view in &mut cells {
                    let basic = cell_view.basic_data();
                    let style = ghostty_cell_style(
                        &cell_view,
                        &basic,
                        default_fg,
                        default_bg,
                        resolved_fg,
                        resolved_bg,
                        palette_overrides.as_ref(),
                    );
                    let symbol = ghostty_buffer_symbol_into(
                        &cell_view,
                        basic.wide,
                        hide_kitty_placeholders,
                        &mut symbol_scratch,
                    );
                    let cell = &mut buf[(area.x + x, area.y + y)];
                    cell.reset();
                    cell.set_symbol(symbol);
                    cell.set_style(style);
                    x += 1;
                }
                while x < area.width {
                    let cell = &mut buf[(area.x + x, area.y + y)];
                    ghostty_reset_cell(cell, default_fg, default_bg);
                    x += 1;
                }
                y = y.saturating_add(1);
            }
            while y < area.height {
                for x in 0..area.width {
                    let cell = &mut buf[(area.x + x, area.y + y)];
                    ghostty_reset_cell(cell, default_fg, default_bg);
                }
                y += 1;
            }
        }

        // A full render draws every row whatever its dirty flag says, so it
        // leaves the flags alone: they belong to dirty-patch collection
        // alone. Clearing them here let a full frame drawn for one purpose
        // swallow rows a later patch still had to send.

        if show_cursor
            && let Some(cursor) =
                cursor_state_from_render_state(render_state, cursor_shape_overridden)
                    .filter(|cursor| cursor.visible)
            && cursor.x < area.width
            && cursor.y < area.height
        {
            frame.set_cursor_position((area.x + cursor.x, area.y + cursor.y));
        }
    }

    pub fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        crate::ghostty::lock_terminal_core(&self.core)
            .ok()
            .map(|mut core| {
                flush_expired_synchronized_output(&mut core);
                if core
                    .terminal
                    .mode_get(crate::ghostty::MODE_SYNCHRONIZED_OUTPUT)
                {
                    return TerminalDirtyPatchOutcome::Fallback;
                }
                #[cfg(test)]
                if let Some(hook) = core.dirty_collection_hook.take() {
                    hook();
                }
                ghostty_collect_dirty_patch(&mut core, area_width, area_height)
            })
            .unwrap_or(TerminalDirtyPatchOutcome::Fallback)
    }
}

/// What the core queued for the pane to deliver.
struct CoreEffects {
    terminal_title_changed: bool,
    clipboard_writes: Vec<Vec<u8>>,
    reported_cwd: Option<std::path::PathBuf>,
    terminal_responses: Vec<Bytes>,
    /// The child set a default colour: the program that did it is to be
    /// looked up once the terminal lock is released
    /// ([`GhosttyPaneTerminal::resolve_default_color_owner`]).
    default_color_owner_pending: bool,
}

/// Collects every effect the core has queued, whichever write or flush
/// produced it, and keeps the default-colour owner bookkeeping in step.
fn collect_core_effects(core: &mut GhosttyPaneCore) -> CoreEffects {
    let terminal_responses = drain_terminal_responses(core);
    let terminal_title_changed = core
        .agent_osc_state
        .apply_terminal_updates(&mut core.terminal);
    let clipboard_writes = core.terminal.take_clipboard_writes();
    let reported_cwd = core
        .terminal
        .take_pwd_changes()
        .into_iter()
        .filter_map(|value| parse_reported_cwd(&value.0))
        .next_back();
    let default_color_owner_pending = note_default_color_change(core);
    CoreEffects {
        terminal_title_changed,
        clipboard_writes,
        reported_cwd,
        terminal_responses,
        default_color_owner_pending,
    }
}

/// Drops queued effects that must never reach the live child or the app
/// (restored history).
fn discard_core_effects(terminal: &mut crate::ghostty::Terminal) {
    let _ = terminal.take_pty_responses();
    let _ = terminal.take_clipboard_writes();
    let _ = terminal.take_pwd_changes();
    let _ = terminal.take_title_update();
    let _ = terminal.take_progress_update();
    let _ = terminal.take_default_color_set();
}

fn has_default_color_override(terminal: &crate::ghostty::Terminal) -> bool {
    terminal
        .default_color_override(crate::ghostty::DefaultColor::Foreground)
        .is_some()
        || terminal
            .default_color_override(crate::ghostty::DefaultColor::Background)
            .is_some()
}

/// Keeps the default-colour owner in step with the core: forgets it once no
/// override is left (the child reset it with OSC 110/111, or RIS), and
/// reports whether the child just set an override whose owner still has to
/// be looked up. The lookup scans `/proc`, so the caller does it after
/// releasing the terminal lock ([`GhosttyPaneTerminal::resolve_default_color_owner`]);
/// `shell_pid` 0 (no child yet) is handled there.
fn note_default_color_change(core: &mut GhosttyPaneCore) -> bool {
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
fn drain_terminal_responses(core: &mut GhosttyPaneCore) -> Vec<Bytes> {
    let responses = core.terminal.take_pty_responses();
    let mut replies = Vec::with_capacity(responses.len());
    for response in responses {
        match response {
            crate::ghostty::PtyResponse::Bytes(bytes) => replies.push(Bytes::from(bytes)),
            crate::ghostty::PtyResponse::ColorQuery(query) => {
                replies.extend(color_query_response(&query));
            }
        }
    }
    replies
}

/// The core resolves every colour (child override, then host default, then
/// built-in); this only picks the reply form. A default colour the child set
/// itself is echoed in the form it asked for; everything else is reported
/// the way shepr reports host colours, ST-terminated. No reply for a
/// default colour nobody has set.
fn color_query_response(query: &crate::ghostty::ColorQuery) -> Option<Bytes> {
    let color = query.core_color()?;
    if query.child_override() {
        return Some(Bytes::from(query.encode(color)));
    }
    let command = match query.target() {
        crate::ghostty::ColorQueryTarget::Foreground => "10".to_owned(),
        crate::ghostty::ColorQueryTarget::Background => "11".to_owned(),
        crate::ghostty::ColorQueryTarget::Cursor => "12".to_owned(),
        crate::ghostty::ColorQueryTarget::Palette(index) => format!("4;{index}"),
    };
    Some(osc_rgb_response(&command, color.r, color.g, color.b))
}

fn flush_expired_synchronized_output(core: &mut GhosttyPaneCore) {
    if core.terminal.flush_expired_synchronized_output() {
        core.synchronized_output_epoch = core.synchronized_output_epoch.wrapping_add(1);
    }
}

fn current_cursor_state(core: &mut GhosttyPaneCore) -> Option<TerminalCursorState> {
    let GhosttyPaneCore {
        terminal,
        render_state,
        ..
    } = core;
    render_state.update(terminal);
    cursor_state_from_render_state(render_state, terminal.cursor_shape_overridden())
}

fn cursor_state_from_render_state(
    render_state: &mut crate::ghostty::RenderState,
    cursor_shape_overridden: bool,
) -> Option<TerminalCursorState> {
    let cursor = render_state.cursor();
    let viewport = cursor.viewport?;
    let shape = if cursor_shape_overridden {
        decscusr_cursor_shape(cursor.visual_style, cursor.blinking)
    } else {
        crate::protocol::CursorShapeParam::Default
    };
    Some(TerminalCursorState {
        x: viewport.x,
        y: viewport.y,
        visible: cursor.visible,
        shape,
    })
}

type VisibleHyperlinks = Vec<((u16, u16), String, String)>;

fn ghostty_collect_dirty_patch(
    core: &mut GhosttyPaneCore,
    area_width: u16,
    area_height: u16,
) -> TerminalDirtyPatchOutcome {
    macro_rules! finish {
        ($outcome:expr) => {{
            return $outcome;
        }};
    }
    macro_rules! fallback {
        ($reason:literal) => {{
            finish!(TerminalDirtyPatchOutcome::Fallback);
        }};
    }

    let host_theme = core.host_terminal_theme;
    let initial_default_foreground = core.initial_default_foreground;
    let initial_default_background = core.initial_default_background;
    let GhosttyPaneCore {
        terminal,
        render_state,
        ..
    } = core;
    render_state.update(terminal);
    match render_state.dirty() {
        crate::ghostty::Dirty::Clean => finish!(TerminalDirtyPatchOutcome::Clean),
        crate::ghostty::Dirty::Partial | crate::ghostty::Dirty::Full => {}
    }

    let colors = render_state.colors();
    let default_bg = ghostty_default_bg(colors.background, host_theme, initial_default_background);
    let default_fg = ghostty_default_fg(colors.foreground, host_theme, initial_default_foreground);
    let resolved_fg = Some(ghostty_color(colors.foreground));
    let resolved_bg = Some(ghostty_color(colors.background));
    let default_palette = terminal.default_palette();
    let palette_overrides = PaletteOverrides::new(&colors.palette, &default_palette);
    // Shepr never renders kitty graphics, but a program may still emit the
    // unicode placeholder codepoint as literal text; always hide it so a
    // stray private-use glyph doesn't leak into the rendered pane.
    let hide_kitty_placeholders = true;

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
            let style = ghostty_cell_style(
                &cell_view,
                &basic,
                default_fg,
                default_bg,
                resolved_fg,
                resolved_bg,
                palette_overrides.as_ref(),
            );
            let symbol = ghostty_buffer_symbol_into(
                &cell_view,
                basic.wide,
                hide_kitty_placeholders,
                &mut symbol_scratch,
            )
            .to_owned();
            patch_cells.push(cell_data_from_style(symbol, style));
            x = x.saturating_add(1);
        }
        while x < area_width {
            patch_cells.push(blank_cell_data(default_fg, default_bg));
            x += 1;
        }
        patch_rows.push((y, patch_cells));
    }

    // Nothing above mutates dirty state. Only clear it after every row has
    // been collected successfully, so a safety fallback leaves the next
    // collection with the same information. Rows below the area were not
    // collected: they stay dirty, and so does the overall state, so it only
    // reads Clean when no row is left to send.
    let mut rows_left = false;
    for row in render_state.iter_rows() {
        if row.y() < area_height {
            row.clear_dirty();
        } else if row.is_dirty() {
            rows_left = true;
        }
    }
    let remaining = if rows_left {
        crate::ghostty::Dirty::Partial
    } else {
        crate::ghostty::Dirty::Clean
    };
    render_state.set_dirty(remaining);

    finish!(TerminalDirtyPatchOutcome::Patch(TerminalDirtyPatch {
        rows: patch_rows
    }));
}

fn ghostty_visible_hyperlinks(
    core: &mut GhosttyPaneCore,
    area: Rect,
) -> Result<VisibleHyperlinks, crate::ghostty::Error> {
    let GhosttyPaneCore {
        terminal,
        render_state,
        ..
    } = core;
    render_state.update(terminal);
    let mut links = Vec::new();
    for row in render_state.iter_rows().take(usize::from(area.height)) {
        let y = row.y();
        for (x, cells) in row.cells().take(usize::from(area.width)).enumerate() {
            let x = u16::try_from(x).unwrap_or(u16::MAX);
            if cells.has_hyperlink()
                && let Some(uri) = terminal.viewport_hyperlink_uri(x, ViewportRow(y))?
            {
                links.push(((area.x + x, area.y + y), ghostty_cell_symbol(&cells), uri));
            }
        }
    }
    Ok(links)
}

fn ghostty_visible_text(core: &mut GhosttyPaneCore) -> String {
    let GhosttyPaneCore {
        terminal,
        render_state,
        ..
    } = core;
    render_state.update(terminal);
    let mut lines: Vec<_> = render_state
        .iter_rows()
        .map(|row| ghostty_line_from_cells(row.cells()))
        .collect();
    trim_trailing_blank_rows(&mut lines);
    lines_to_text(&lines)
}

fn ghostty_visible_ansi(core: &GhosttyPaneCore) -> Result<String, crate::ghostty::Error> {
    let rows = core.terminal.rows();
    let cols = core.terminal.cols();
    if rows == 0 || cols == 0 {
        return Ok(String::new());
    }
    core.terminal.read_ansi_viewport(
        Point::new(ViewportRow(0), 0),
        Point::new(ViewportRow(rows.saturating_sub(1)), cols.saturating_sub(1)),
        false,
    )
}

/// The detector's snapshot: the active screen's rows up to the last content
/// (or cursor) row, never anything above the screen. After ED2, Ctrl-L or an
/// agent redrawing from the top, alacritty has pushed the previous frame into
/// history; reading a screen's worth of rows ending at the last content row
/// would hand the detector that stale frame (an old "proceed?" blocker, say).
fn ghostty_detection_text(core: &mut GhosttyPaneCore) -> Result<String, crate::ghostty::Error> {
    let terminal = &core.terminal;
    let screen_rows = usize::from(terminal.rows()).max(1);
    let Some((start, end, _)) = ghostty_recent_read_range(terminal, screen_rows)? else {
        return Ok(String::new());
    };
    let screen_start = terminal.total_rows().saturating_sub(screen_rows);
    ghostty_text_rows(terminal, start.max(screen_start), end, screen_rows)
}

fn ghostty_recent_text_snapshot(
    core: &mut GhosttyPaneCore,
    lines: usize,
) -> Result<TerminalReadSnapshot, crate::ghostty::Error> {
    let terminal = &core.terminal;
    let Some((start, end, _)) = ghostty_recent_read_range(terminal, lines)? else {
        return Ok(TerminalReadSnapshot::default());
    };
    let text = ghostty_text_rows(terminal, start, end, lines)?;
    Ok(finish_recent_snapshot(text, start))
}

fn ghostty_recent_text_unwrapped_snapshot(
    core: &mut GhosttyPaneCore,
    lines: usize,
) -> Result<TerminalReadSnapshot, crate::ghostty::Error> {
    let terminal = &core.terminal;
    let Some((start, end, cols)) = ghostty_recent_read_range(terminal, lines)? else {
        return Ok(TerminalReadSnapshot::default());
    };
    let text = terminal.read_text_screen(
        Point::new(ScreenRow(start), 0),
        Point::new(ScreenRow(end), cols.saturating_sub(1)),
        false,
    )?;
    Ok(finish_recent_snapshot(text, start))
}

fn ghostty_recent_ansi_snapshot(
    core: &mut GhosttyPaneCore,
    lines: usize,
    unwrap: bool,
) -> Result<TerminalReadSnapshot, crate::ghostty::Error> {
    let terminal = &core.terminal;
    let Some((start, end, cols)) = ghostty_recent_read_range(terminal, lines)? else {
        return Ok(TerminalReadSnapshot::default());
    };
    let text = terminal.read_ansi_screen(
        Point::new(ScreenRow(start), 0),
        Point::new(ScreenRow(end), cols.saturating_sub(1)),
        false,
        unwrap,
    )?;
    Ok(finish_recent_snapshot(text, start))
}

/// Recent read limits are measured in rendered rows, including blank or styled
/// rows. The read is truncated only when rows above its first row were left
/// out; trailing blank rows below the content are not "omitted" history.
fn finish_recent_snapshot(text: String, start: usize) -> TerminalReadSnapshot {
    TerminalReadSnapshot {
        text,
        truncated: start > 0,
    }
}

fn ghostty_text_rows(
    terminal: &crate::ghostty::Terminal,
    start: usize,
    end: usize,
    lines: usize,
) -> Result<String, crate::ghostty::Error> {
    let mut rows = Vec::with_capacity(end.saturating_sub(start).saturating_add(1));
    let mut scratch = String::new();
    for y in start..=end {
        let mut row = String::new();
        ghostty_screen_row_into(terminal, ScreenRow(y), &mut scratch, &mut row);
        rows.push(row);
    }
    trim_trailing_blank_rows(&mut rows);
    Ok(recent_text_from_rows(&rows, lines))
}

/// The screen rows a recent read covers, on the active screen: while the
/// alternate screen is active that is the full-screen program's frame, never
/// the primary history (alacritty offers no access to the inactive grid).
/// History persistence must not take that for history; it reads through
/// [`PaneTerminal::primary_history_ansi`], which says so instead.
fn ghostty_recent_read_range(
    terminal: &crate::ghostty::Terminal,
    lines: usize,
) -> Result<Option<(usize, usize, u16)>, crate::ghostty::Error> {
    let total_rows = terminal.total_rows();
    let cols = terminal.cols();
    if total_rows == 0 || cols == 0 || lines == 0 {
        return Ok(None);
    }

    let physical_end = total_rows.saturating_sub(1);
    if terminal.active_screen() != crate::ghostty::ActiveScreen::Primary {
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
        ghostty_screen_row_into(terminal, ScreenRow(row), &mut scratch, &mut text);
        if !text.trim().is_empty() {
            last_content_row = Some(row);
            break;
        }
    }
    let end = last_content_row
        .map(|row| row.max(cursor_row))
        .unwrap_or_else(|| total_rows.saturating_sub(1));
    let start = end.saturating_add(1).saturating_sub(lines);
    Ok(Some((start, end, cols)))
}

fn terminal_scroll_metrics(terminal: &crate::ghostty::Terminal) -> ScrollMetrics {
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

fn ghostty_set_scroll_offset_from_bottom(
    terminal: &mut crate::ghostty::Terminal,
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

fn ghostty_extract_selection(
    core: &mut GhosttyPaneCore,
    selection: &crate::selection::Selection,
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
            false,
        )
        .ok()
}

/// Writes screen row `y`'s plain text into `line`, trailing blanks trimmed
/// (empty for a row that is not retained). Straight from the grid, with no
/// per-cell copies: this runs per detection tick for every agent pane.
fn ghostty_screen_row_into(
    terminal: &crate::ghostty::Terminal,
    y: ScreenRow,
    scratch: &mut String,
    line: &mut String,
) {
    line.clear();
    terminal.visit_screen_row_text(y, scratch, |_, wide, text| {
        if wide != crate::ghostty::CellWide::SpacerTail {
            line.push_str(text);
        }
    });
    line.truncate(line.trim_end().len());
}

fn ghostty_line_from_cells<'a>(
    cells: impl Iterator<Item = crate::ghostty::CellView<'a>>,
) -> String {
    let mut line = String::new();
    for cell in cells {
        line.push_str(&ghostty_cell_symbol(&cell));
    }
    line.trim_end().to_string()
}

fn ghostty_cell_symbol(cells: &crate::ghostty::CellView<'_>) -> String {
    if cells.wide() == crate::ghostty::CellWide::SpacerTail {
        return String::new();
    }
    let text = cells.grapheme_text();
    if text.chars().next().map(u32::from) == Some(crate::ghostty::KITTY_UNICODE_PLACEHOLDER) {
        return " ".to_string();
    }
    if text.is_empty() {
        return " ".to_string();
    }
    text
}

pub(super) fn ghostty_blank_symbol_for_width(wide: crate::ghostty::CellWide) -> &'static str {
    match wide {
        crate::ghostty::CellWide::Wide => "  ",
        crate::ghostty::CellWide::SpacerTail => "",
        crate::ghostty::CellWide::Narrow | crate::ghostty::CellWide::SpacerHead => " ",
    }
}

#[cfg(test)]
pub(super) fn ghostty_normalize_buffer_symbol(
    symbol: &str,
    wide: crate::ghostty::CellWide,
) -> String {
    let expected_width = match wide {
        crate::ghostty::CellWide::Wide => 2,
        crate::ghostty::CellWide::Narrow | crate::ghostty::CellWide::SpacerHead => 1,
        crate::ghostty::CellWide::SpacerTail => 0,
    };
    let actual_width = symbol.width();
    if actual_width == expected_width {
        return symbol.to_string();
    }

    if wide == crate::ghostty::CellWide::Narrow && actual_width == 2 {
        return symbol.to_string();
    }
    if wide == crate::ghostty::CellWide::Narrow && is_halfwidth_katakana_voiced_mark(symbol) {
        return symbol.to_string();
    }
    if wide == crate::ghostty::CellWide::Wide && is_halfwidth_katakana_voiced_grapheme(symbol) {
        return symbol.to_string();
    }

    ghostty_blank_symbol_for_width(wide).to_string()
}

/// U+FF9E/U+FF9F on their own. unicode-width measures them as zero-width, but
/// the terminal core gives them a cell (as wcwidth does), so they are kept.
fn is_halfwidth_katakana_voiced_mark(symbol: &str) -> bool {
    matches!(symbol, "\u{ff9e}" | "\u{ff9f}")
}

fn is_halfwidth_katakana_voiced_grapheme(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    let Some(base) = chars.next() else {
        return false;
    };
    let Some(mark) = chars.next() else {
        return false;
    };
    chars.next().is_none()
        && ('\u{ff66}'..='\u{ff9d}').contains(&base)
        && matches!(mark, '\u{ff9e}' | '\u{ff9f}')
}

fn ghostty_buffer_symbol_into<'a>(
    cells: &crate::ghostty::CellView<'_>,
    wide: crate::ghostty::CellWide,
    hide_kitty_placeholders: bool,
    symbol_scratch: &'a mut String,
) -> &'a str {
    symbol_scratch.clear();
    match wide {
        crate::ghostty::CellWide::SpacerTail => {}
        crate::ghostty::CellWide::SpacerHead => symbol_scratch.push(' '),
        crate::ghostty::CellWide::Narrow | crate::ghostty::CellWide::Wide => {
            cells.grapheme_text_into(symbol_scratch);
            let hidden_kitty_placeholder = hide_kitty_placeholders
                && symbol_scratch.chars().next().map(u32::from)
                    == Some(crate::ghostty::KITTY_UNICODE_PLACEHOLDER);
            if hidden_kitty_placeholder || symbol_scratch.is_empty() {
                symbol_scratch.clear();
                symbol_scratch.push(' ');
            }
        }
    }

    let expected_width = match wide {
        crate::ghostty::CellWide::Wide => 2,
        crate::ghostty::CellWide::Narrow | crate::ghostty::CellWide::SpacerHead => 1,
        crate::ghostty::CellWide::SpacerTail => 0,
    };
    let actual_width = symbol_scratch.width();
    if actual_width != expected_width
        && !(wide == crate::ghostty::CellWide::Narrow && actual_width == 2)
        && !(wide == crate::ghostty::CellWide::Narrow
            && is_halfwidth_katakana_voiced_mark(symbol_scratch))
        && !(wide == crate::ghostty::CellWide::Wide
            && is_halfwidth_katakana_voiced_grapheme(symbol_scratch))
    {
        symbol_scratch.clear();
        symbol_scratch.push_str(ghostty_blank_symbol_for_width(wide));
    }

    symbol_scratch.as_str()
}

fn ghostty_reset_cell(
    cell: &mut ratatui::buffer::Cell,
    default_fg: Option<Color>,
    default_bg: Option<Color>,
) {
    cell.reset();
    cell.set_symbol(" ");
    if let Some(bg) = default_bg {
        cell.set_bg(bg);
    }
    if let Some(fg) = default_fg {
        cell.set_fg(fg);
    }
}

fn blank_cell_data(default_fg: Option<Color>, default_bg: Option<Color>) -> CellData {
    cell_data_from_style(
        " ".to_string(),
        ghostty_default_style(default_fg, default_bg),
    )
}

fn cell_data_from_style(symbol: String, style: Style) -> CellData {
    CellData {
        symbol,
        fg: crate::protocol::WireColor::from_ratatui(style.fg.unwrap_or(Color::Reset)),
        bg: crate::protocol::WireColor::from_ratatui(style.bg.unwrap_or(Color::Reset)),
        style: crate::protocol::WireStyle::from_ratatui_modifier(style.add_modifier),
        skip: false,
        hyperlink: None,
    }
}

fn ghostty_default_style(default_fg: Option<Color>, default_bg: Option<Color>) -> Style {
    let mut style = Style::default();
    if let Some(fg) = default_fg {
        style = style.fg(fg);
    }
    if let Some(bg) = default_bg {
        style = style.bg(bg);
    }
    style
}

fn ghostty_cell_style(
    cells: &crate::ghostty::CellView<'_>,
    basic: &crate::ghostty::CellBasicData,
    default_fg: Option<Color>,
    default_bg: Option<Color>,
    resolved_fg: Option<Color>,
    resolved_bg: Option<Color>,
    palette_overrides: Option<&PaletteOverrides>,
) -> Style {
    let mut fg = basic
        .style
        .fg_color
        .map(|color| ghostty_cell_color(color, palette_overrides))
        .or_else(|| cells.fg_color().map(ghostty_color))
        .or(default_fg);
    let mut bg = basic
        .style
        .bg_color
        .map(|color| ghostty_cell_color(color, palette_overrides))
        .or_else(|| cells.bg_color().map(ghostty_color))
        .or(default_bg);
    if basic.style.invisible {
        fg = bg.or(default_bg);
    }
    if basic.style.inverse {
        // When the background is transparent (None), resolve it to the
        // actual terminal background color before swapping.  Otherwise
        // the swapped fg becomes None (Color::Reset) which the host
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

    let mut style = ghostty_default_style(fg, bg);
    if let Some(underline_color) = basic
        .style
        .underline_color
        .map(|color| ghostty_cell_color(color, palette_overrides))
    {
        style = style.underline_color(underline_color);
    }
    let mut flags = crate::protocol::WireStyleFlags::default();
    if basic.style.bold {
        flags = flags.union(crate::protocol::WireStyleFlags::BOLD);
    }
    if basic.style.faint {
        flags = flags.union(crate::protocol::WireStyleFlags::DIM);
    }
    if basic.style.italic {
        flags = flags.union(crate::protocol::WireStyleFlags::ITALIC);
    }
    if basic.style.strikethrough {
        flags = flags.union(crate::protocol::WireStyleFlags::CROSSED_OUT);
    }
    let wire_style = crate::protocol::WireStyle {
        flags,
        underline: basic.style.underline,
    };
    style.add_modifier(wire_style.to_ratatui_modifier())
}

fn osc_rgb_response(command: &str, r: u8, g: u8, b: u8) -> Bytes {
    let r = u16::from(r) * 257;
    let g = u16::from(g) * 257;
    let b = u16::from(b) * 257;
    Bytes::from(format!("\x1b]{command};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"))
}

fn ghostty_default_fg(
    color: crate::ghostty::RgbColor,
    host_theme: crate::host_term::theme::TerminalTheme,
    initial_default_foreground: Option<crate::ghostty::RgbColor>,
) -> Option<Color> {
    if let Some(host_foreground) = host_theme.foreground {
        if host_foreground == color {
            None
        } else {
            Some(ghostty_color(color))
        }
    } else if initial_default_foreground.is_some_and(|initial| initial != color) {
        Some(ghostty_color(color))
    } else {
        None
    }
}

fn ghostty_default_bg(
    color: crate::ghostty::RgbColor,
    host_theme: crate::host_term::theme::TerminalTheme,
    initial_default_background: Option<crate::ghostty::RgbColor>,
) -> Option<Color> {
    if let Some(host_background) = host_theme.background {
        if host_background == color {
            None
        } else {
            Some(ghostty_color(color))
        }
    } else if initial_default_background.is_some_and(|initial| initial != color) {
        Some(ghostty_color(color))
    } else {
        None
    }
}

// Palette entries the program redefined with OSC 4. Forwarding a palette index to the
// host makes it resolve against the host's own palette, discarding the redefinition.
// Only overridden entries become RGB; the rest stay indexed and keep following the
// host theme. None when nothing was redefined, which is the common case.
struct PaletteOverrides([Option<crate::ghostty::RgbColor>; 256]);

impl PaletteOverrides {
    fn new(
        active: &[crate::ghostty::RgbColor; 256],
        default: &[crate::ghostty::RgbColor; 256],
    ) -> Option<Self> {
        let mut overrides = [None; 256];
        let mut any = false;
        for (index, (active, default)) in active.iter().zip(default.iter()).enumerate() {
            if active != default {
                overrides[index] = Some(*active);
                any = true;
            }
        }
        any.then_some(Self(overrides))
    }

    fn get(&self, index: u8) -> Option<crate::ghostty::RgbColor> {
        self.0[usize::from(index)]
    }
}

fn ghostty_cell_color(
    color: crate::ghostty::CellColor,
    palette_overrides: Option<&PaletteOverrides>,
) -> Color {
    match color {
        crate::ghostty::CellColor::Palette(index) => {
            match palette_overrides.and_then(|overrides| overrides.get(index)) {
                Some(color) => ghostty_color(color),
                None => Color::Indexed(index),
            }
        }
        crate::ghostty::CellColor::Rgb(color) => ghostty_color(color),
    }
}

fn ghostty_color(color: crate::ghostty::RgbColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn lines_to_text(lines: &[String]) -> String {
    let text = lines.join("\n");
    if text.is_empty() {
        text
    } else {
        format!("{text}\n")
    }
}

pub(super) fn trim_trailing_blank_rows(rows: &mut Vec<String>) {
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
}

fn recent_text_from_rows(rows: &[String], lines: usize) -> String {
    let start = rows.len().saturating_sub(lines);
    let text = rows[start..].join("\n");
    if text.is_empty() {
        text
    } else {
        format!("{text}\n")
    }
}

fn should_probe_host_terminal_theme_restore(core: &GhosttyPaneCore) -> bool {
    if core.transient_default_color_owner_pgid.is_none() || core.host_terminal_theme.is_empty() {
        return false;
    }

    core.terminal.active_screen() != crate::ghostty::ActiveScreen::Alternate
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{layout::Rect, style::Color};

    #[test]
    fn plain_page_keys_host_scroll_for_shell_like_decckm_with_bracketed_paste() {
        assert!(
            InputState {
                alternate_screen: false,
                application_cursor: true,
                bracketed_paste: true,
                focus_reporting: false,
                mouse_protocol_mode: crate::input::MouseProtocolMode::None,
                mouse_protocol_encoding: crate::input::MouseProtocolEncoding::Default,
                mouse_alternate_scroll: false,
                modify_other_keys: false,
                color_scheme_reporting: false,
            }
            .plain_page_keys_use_host_scrollback()
        );
    }

    fn text_cell(text: &str) -> crate::ghostty::ScreenTextCell {
        crate::ghostty::ScreenTextCell {
            wide: crate::ghostty::CellWide::Narrow,
            graphemes: text.chars().map(u32::from).collect(),
        }
    }

    fn rgb(r: u8, g: u8, b: u8) -> crate::ghostty::RgbColor {
        crate::ghostty::RgbColor { r, g, b }
    }

    #[test]
    fn dirty_full_collects_bounded_viewport_patch() {
        let mut terminal = crate::ghostty::Terminal::new(4, 3, 200);
        terminal.write(b"one\r\ntwo\r\nthree");
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));

        let patch = match pane.collect_dirty_patch(4, 3) {
            TerminalDirtyPatchOutcome::Patch(patch) => patch,
            outcome => panic!("expected viewport patch, got {outcome:?}"),
        };

        assert_eq!(patch.rows.len(), 3);
        assert_eq!(
            patch.rows.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(patch.rows.iter().all(|(_, cells)| cells.len() == 4));
        assert!(matches!(
            pane.collect_dirty_patch(4, 3),
            TerminalDirtyPatchOutcome::Clean
        ));
    }

    #[test]
    fn palette_overrides_are_none_without_an_osc4_write() {
        let default = [rgb(1, 2, 3); 256];
        assert!(PaletteOverrides::new(&default, &default).is_none());
    }

    #[test]
    fn redefined_palette_entries_render_as_rgb_and_others_stay_indexed() {
        let default = [rgb(1, 2, 3); 256];
        let mut active = default;
        active[18] = rgb(169, 177, 214);
        let overrides = PaletteOverrides::new(&active, &default).expect("index 18 differs");

        assert_eq!(
            ghostty_cell_color(crate::ghostty::CellColor::Palette(18), Some(&overrides)),
            Color::Rgb(169, 177, 214)
        );
        // Untouched entries keep being forwarded, so they still follow the host theme.
        assert_eq!(
            ghostty_cell_color(crate::ghostty::CellColor::Palette(19), Some(&overrides)),
            Color::Indexed(19)
        );
        // ...and so does everything when the program never wrote a palette at all.
        assert_eq!(
            ghostty_cell_color(crate::ghostty::CellColor::Palette(18), None),
            Color::Indexed(18)
        );
    }

    #[test]
    fn direct_rgb_cells_are_unaffected_by_palette_overrides() {
        let default = [rgb(1, 2, 3); 256];
        let mut active = default;
        active[18] = rgb(169, 177, 214);
        let overrides = PaletteOverrides::new(&active, &default).expect("index 18 differs");
        assert_eq!(
            ghostty_cell_color(
                crate::ghostty::CellColor::Rgb(rgb(122, 162, 247)),
                Some(&overrides)
            ),
            Color::Rgb(122, 162, 247)
        );
    }

    fn wide_text_cells(text: &str) -> [crate::ghostty::ScreenTextCell; 2] {
        [
            crate::ghostty::ScreenTextCell {
                wide: crate::ghostty::CellWide::Wide,
                graphemes: text.chars().map(u32::from).collect(),
            },
            crate::ghostty::ScreenTextCell {
                wide: crate::ghostty::CellWide::SpacerTail,
                graphemes: Vec::new(),
            },
        ]
    }

    fn text_row(
        cells: impl IntoIterator<Item = crate::ghostty::ScreenTextCell>,
        soft_wrapped: bool,
    ) -> crate::ghostty::ScreenTextRow {
        crate::ghostty::ScreenTextRow {
            cells: cells.into_iter().collect(),
            soft_wrapped,
            wrap_continuation: false,
        }
    }

    fn search_primary(
        buffer: &RetainedTextBuffer,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<TerminalTextMatch<AbsRow>> {
        buffer
            .search_window(
                query,
                case_sensitive,
                crate::ghostty::ActiveScreen::Primary,
                TerminalSearchDirection::Forward,
                TerminalTextPoint {
                    row: AbsRow(0),
                    col: 0,
                },
                None,
                usize::MAX,
            )
            .matches
    }

    fn write_numbered_lines(terminal: &mut crate::ghostty::Terminal, count: usize) {
        for i in 0..count {
            terminal.write(format!("{i:06}\r\n").as_bytes());
        }
    }

    fn write_wrapped_contract_lines(terminal: &mut crate::ghostty::Terminal, count: usize) {
        for i in 0..count {
            terminal.write(format!("WRAP-{i:03}-abcdefghijklmnopqrstuvwxyz\r\n").as_bytes());
        }
        terminal.write(b"END");
    }

    #[test]
    fn retained_text_search_crosses_soft_wraps_but_not_hard_lines() {
        let buffer = RetainedTextBuffer::new(
            5,
            vec![
                text_row("abcde".chars().map(|ch| text_cell(&ch.to_string())), true),
                text_row("fgh  ".chars().map(|ch| text_cell(&ch.to_string())), false),
                text_row("abc  ".chars().map(|ch| text_cell(&ch.to_string())), false),
            ],
        );

        let matches = search_primary(&buffer, "def", true);
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].start,
            TerminalTextPoint {
                row: AbsRow(0),
                col: 3
            }
        );
        assert_eq!(
            matches[0].end,
            TerminalTextPoint {
                row: AbsRow(1),
                col: 0
            }
        );
        assert!(search_primary(&buffer, "hab", true).is_empty());
    }

    #[test]
    fn retained_text_search_maps_wide_and_combining_graphemes_to_cells() {
        let mut cells = vec![text_cell("A")];
        cells.extend(wide_text_cells("界"));
        cells.push(text_cell("e\u{301}"));
        cells.push(text_cell("Z"));
        let buffer = RetainedTextBuffer::new(5, vec![text_row(cells, false)]);

        let matches = search_primary(&buffer, "界e\u{301}", true);
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].start,
            TerminalTextPoint {
                row: AbsRow(0),
                col: 1
            }
        );
        assert_eq!(
            matches[0].end,
            TerminalTextPoint {
                row: AbsRow(0),
                col: 3
            }
        );
        assert!(search_primary(&buffer, "\u{301}", true).is_empty());
    }

    #[test]
    fn retained_text_search_skips_wide_spacer_heads_at_soft_wraps() {
        let mut first = "abcd"
            .chars()
            .map(|ch| text_cell(&ch.to_string()))
            .collect::<Vec<_>>();
        first.push(crate::ghostty::ScreenTextCell {
            wide: crate::ghostty::CellWide::SpacerHead,
            graphemes: Vec::new(),
        });
        let mut second = wide_text_cells("界").to_vec();
        second.extend("xyz".chars().map(|ch| text_cell(&ch.to_string())));
        let buffer =
            RetainedTextBuffer::new(5, vec![text_row(first, true), text_row(second, false)]);

        let matches = search_primary(&buffer, "d界", true);
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].start,
            TerminalTextPoint {
                row: AbsRow(0),
                col: 3
            }
        );
        assert_eq!(
            matches[0].end,
            TerminalTextPoint {
                row: AbsRow(1),
                col: 1
            }
        );
    }

    #[test]
    fn retained_text_word_motion_does_not_split_at_a_wide_spacer_head() {
        let mut first = "abcd"
            .chars()
            .map(|ch| text_cell(&ch.to_string()))
            .collect::<Vec<_>>();
        first.push(crate::ghostty::ScreenTextCell {
            wide: crate::ghostty::CellWide::SpacerHead,
            graphemes: Vec::new(),
        });
        let mut second = wide_text_cells("界").to_vec();
        second.extend("xyz".chars().map(|ch| text_cell(&ch.to_string())));
        let buffer =
            RetainedTextBuffer::new(5, vec![text_row(first, true), text_row(second, false)]);

        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextStart),
            None
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextEnd),
            Some(TerminalTextPoint {
                row: AbsRow(1),
                col: 4
            })
        );
    }

    #[test]
    fn retained_text_search_is_literal_and_unicode_case_aware() {
        let buffer = RetainedTextBuffer::new(
            12,
            vec![text_row(
                "CAFÉ a.b    ".chars().map(|ch| text_cell(&ch.to_string())),
                false,
            )],
        );

        assert_eq!(search_primary(&buffer, "café", false).len(), 1);
        assert!(search_primary(&buffer, "café", true).is_empty());
        assert_eq!(search_primary(&buffer, "a.b", true).len(), 1);
        assert!(search_primary(&buffer, "a?b", true).is_empty());
    }

    #[test]
    fn retained_text_word_motions_use_tmux_separators_across_rows() {
        let buffer = RetainedTextBuffer::new(
            6,
            vec![
                text_row("a_b.c ".chars().map(|ch| text_cell(&ch.to_string())), false),
                text_row(
                    "\u{2014}d    ".chars().map(|ch| text_cell(&ch.to_string())),
                    false,
                ),
            ],
        );

        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 3
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 3, TerminalWordMotion::NextStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 4
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 4, TerminalWordMotion::NextStart),
            Some(TerminalTextPoint {
                row: AbsRow(1),
                col: 0
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(1), 1, TerminalWordMotion::PreviousStart),
            Some(TerminalTextPoint {
                row: AbsRow(1),
                col: 0
            })
        );
    }

    #[test]
    fn retained_text_big_word_motions_treat_only_whitespace_as_separators() {
        let buffer = RetainedTextBuffer::new(
            20,
            vec![text_row(
                "foo.bar baz qux/quux"
                    .chars()
                    .map(|ch| text_cell(&ch.to_string())),
                false,
            )],
        );

        // `W` skips punctuation-separated segments and lands on the next
        // whitespace-delimited run.
        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 8
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 8, TerminalWordMotion::NextBigStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 12
            })
        );
        // `E` lands on the last character of the current/next run.
        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigEnd),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 6
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 6, TerminalWordMotion::NextBigEnd),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 10
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 12, TerminalWordMotion::NextBigEnd),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 19
            })
        );
        // `B` returns to the beginning of the previous run.
        assert_eq!(
            buffer.word_motion(AbsRow(0), 19, TerminalWordMotion::PreviousBigStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 12
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 12, TerminalWordMotion::PreviousBigStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 8
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 8, TerminalWordMotion::PreviousBigStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 0
            })
        );

        // Lowercase motions keep their punctuation-aware behavior.
        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 3
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 3, TerminalWordMotion::NextStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 4
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 4, TerminalWordMotion::PreviousStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 3
            })
        );
    }

    #[test]
    fn retained_text_big_word_motions_cross_rows_and_blank_lines() {
        let buffer = RetainedTextBuffer::new(
            6,
            vec![
                text_row("a.b-c ".chars().map(|ch| text_cell(&ch.to_string())), false),
                text_row("      ".chars().map(|ch| text_cell(&ch.to_string())), false),
                text_row("d_e   ".chars().map(|ch| text_cell(&ch.to_string())), false),
            ],
        );

        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigStart),
            Some(TerminalTextPoint {
                row: AbsRow(2),
                col: 0
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(2), 0, TerminalWordMotion::PreviousBigStart),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 0
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 0, TerminalWordMotion::NextBigEnd),
            Some(TerminalTextPoint {
                row: AbsRow(0),
                col: 4
            })
        );
        assert_eq!(
            buffer.word_motion(AbsRow(0), 4, TerminalWordMotion::NextBigEnd),
            Some(TerminalTextPoint {
                row: AbsRow(2),
                col: 2
            })
        );
    }

    #[test]
    fn live_terminal_word_motion_expands_across_long_blank_history() {
        let mut terminal = crate::ghostty::Terminal::new(10, 3, 200);
        terminal.write(b"origin\r\n");
        for _ in 0..80 {
            terminal.write(b"\r\n");
        }
        let last_row = ScreenRow(terminal.total_rows().saturating_sub(1));
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));

        assert_eq!(
            pane.word_motion_target(last_row, 0, TerminalWordMotion::PreviousStart),
            Some(TerminalTextPoint {
                row: ScreenRow(0),
                col: 0,
            })
        );
    }

    #[test]
    fn live_terminal_word_end_expands_through_a_long_soft_wrap() {
        let mut terminal = crate::ghostty::Terminal::new(2, 3, 200);
        let word = "a".repeat(132);
        terminal.write(word.as_bytes());
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
        let text_match = pane
            .search_text_window(
                &word,
                true,
                TerminalSearchDirection::Forward,
                TerminalTextPoint {
                    row: ScreenRow(0),
                    col: 0,
                },
                None,
                1,
            )
            .matches[0];

        assert_eq!(
            pane.word_motion_target(
                text_match.start.row,
                text_match.start.col,
                TerminalWordMotion::NextEnd,
            ),
            Some(text_match.end)
        );
    }

    #[test]
    fn live_terminal_word_end_expands_through_a_long_wide_soft_wrap() {
        let mut terminal = crate::ghostty::Terminal::new(2, 3, 200);
        let word = "界".repeat(66);
        terminal.write(word.as_bytes());
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
        let text_match = pane
            .search_text_window(
                &word,
                true,
                TerminalSearchDirection::Forward,
                TerminalTextPoint {
                    row: ScreenRow(0),
                    col: 0,
                },
                None,
                1,
            )
            .matches[0];

        // The word end sits on the head cell of the final wide glyph, past the
        // initial read window, so the window has to expand to reach it.
        assert_eq!(
            pane.word_motion_target(
                text_match.start.row,
                text_match.start.col,
                TerminalWordMotion::NextEnd,
            ),
            Some(TerminalTextPoint {
                row: text_match.end.row,
                col: 0,
            })
        );
    }

    fn current_palette_color(pane: &GhosttyPaneTerminal, index: u8) -> crate::ghostty::RgbColor {
        let mut core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
        let GhosttyPaneCore {
            terminal,
            render_state,
            ..
        } = &mut *core;
        render_state.update(terminal);
        render_state.colors().palette[usize::from(index)]
    }

    fn expected_osc_rgb_response(command: &str, color: crate::ghostty::RgbColor) -> Bytes {
        let r = u16::from(color.r) * 257;
        let g = u16::from(color.g) * 257;
        let b = u16::from(color.b) * 257;
        Bytes::from(format!("\x1b]{command};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"))
    }

    #[test]
    fn process_pty_bytes_reports_latest_working_directory_report() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 100);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let partial = pane.process_pty_bytes(pane_id, 0, b"\x1b]7;file:///tmp/shepr%20");
        assert_eq!(partial.reported_cwd, None);

        let completed = pane.process_pty_bytes(pane_id, 0, b"repo\x07");
        assert_eq!(
            completed.reported_cwd,
            Some(std::path::PathBuf::from("/tmp/shepr repo"))
        );

        let latest = pane.process_pty_bytes(
            pane_id,
            0,
            b"\x1b]9;9;/tmp/conemu\x1b\\\x1b]1337;CurrentDir=/tmp/iterm2\x1b\\",
        );
        assert_eq!(
            latest.reported_cwd,
            Some(std::path::PathBuf::from("/tmp/iterm2"))
        );
    }

    #[test]
    fn process_pty_bytes_reports_only_completed_title_changes() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 100);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        assert!(
            !pane
                .process_pty_bytes(pane_id, 0, b"\x1b]0;buil")
                .terminal_title_changed
        );
        assert!(
            pane.process_pty_bytes(pane_id, 0, b"ding\x07")
                .terminal_title_changed
        );
        assert!(
            !pane
                .process_pty_bytes(pane_id, 0, b"\x1b]2;building\x07")
                .terminal_title_changed
        );
        assert!(
            pane.process_pty_bytes(pane_id, 0, b"\x1b]2;done\x07")
                .terminal_title_changed
        );
    }

    #[test]
    fn process_pty_bytes_surfaces_clipboard_writes_without_other_results() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 100);
        let pane = GhosttyPaneTerminal::new(terminal);

        let result =
            pane.process_pty_bytes(PaneId::from_raw(1), 0, b"output\x1b]52;c;Y2xpcGJvYXJk\x07");

        assert!(result.request_render);
        assert_eq!(result.render_delay, None);
        assert_eq!(result.clipboard_writes, vec![b"clipboard".to_vec()]);
        assert_eq!(result.reported_cwd, None);
        assert!(result.terminal_responses.is_empty());
    }

    #[test]
    fn seeded_history_clipboard_write_does_not_leak_into_live_output() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 100);
        let pane = GhosttyPaneTerminal::new(terminal);
        pane.seed_history_ansi("\x1b]52;c;c3RhbGU=\x07");

        let result = pane.process_pty_bytes(PaneId::from_raw(1), 0, b"live output");

        assert!(result.clipboard_writes.is_empty());
    }

    #[test]
    fn seeded_history_pwd_does_not_leak_into_live_output() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 100);
        let pane = GhosttyPaneTerminal::new(terminal);
        pane.seed_history_ansi("\x1b]7;file:///tmp/restored\x07");

        let result = pane.process_pty_bytes(PaneId::from_raw(1), 0, b"live output");

        assert_eq!(result.reported_cwd, None);
    }

    fn expected_xtgettcap_response(cap_hex: &str, value: Option<&[u8]>) -> Bytes {
        let mut response = format!("\x1bP1+r{cap_hex}").into_bytes();
        if let Some(value) = value {
            response.push(b'=');
            append_upper_hex(value, &mut response);
        }
        response.extend_from_slice(b"\x1b\\");
        Bytes::from(response)
    }

    fn append_upper_hex(bytes: &[u8], output: &mut Vec<u8>) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        for &byte in bytes {
            output.push(HEX[usize::from(byte >> 4)]);
            output.push(HEX[usize::from(byte & 0x0f)]);
        }
    }

    #[test]
    fn decscusr_cursor_shape_preserves_blinking_variants() {
        assert_eq!(
            decscusr_cursor_shape(crate::ghostty::CursorVisualStyle::Block, true),
            crate::protocol::CursorShapeParam::BlinkingBlock
        );
        assert_eq!(
            decscusr_cursor_shape(crate::ghostty::CursorVisualStyle::Block, false),
            crate::protocol::CursorShapeParam::SteadyBlock
        );
        assert_eq!(
            decscusr_cursor_shape(crate::ghostty::CursorVisualStyle::Underline, true),
            crate::protocol::CursorShapeParam::BlinkingUnderline
        );
        assert_eq!(
            decscusr_cursor_shape(crate::ghostty::CursorVisualStyle::Underline, false),
            crate::protocol::CursorShapeParam::SteadyUnderline
        );
        assert_eq!(
            decscusr_cursor_shape(crate::ghostty::CursorVisualStyle::Bar, true),
            crate::protocol::CursorShapeParam::BlinkingBar
        );
        assert_eq!(
            decscusr_cursor_shape(crate::ghostty::CursorVisualStyle::Bar, false),
            crate::protocol::CursorShapeParam::SteadyBar
        );
        assert_eq!(
            decscusr_cursor_shape(crate::ghostty::CursorVisualStyle::BlockHollow, false),
            crate::protocol::CursorShapeParam::SteadyBlock
        );
    }

    #[test]
    fn cursor_state_uses_terminal_default_until_child_sets_shape() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        assert_eq!(
            pane.cursor_state().expect("test precondition").shape,
            crate::protocol::CursorShapeParam::Default
        );

        pane.process_pty_bytes(pane_id, 0, b"\x1b[6 q");

        assert_eq!(
            pane.cursor_state().expect("test precondition").shape,
            crate::protocol::CursorShapeParam::SteadyBar
        );
    }

    #[test]
    fn cursor_state_returns_terminal_default_after_decscusr_reset() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"\x1b[2 q");
        assert_eq!(
            pane.cursor_state().expect("test precondition").shape,
            crate::protocol::CursorShapeParam::SteadyBlock
        );

        pane.process_pty_bytes(pane_id, 0, b"\x1b[0 q");

        assert_eq!(
            pane.cursor_state().expect("test precondition").shape,
            crate::protocol::CursorShapeParam::Default
        );
    }

    #[test]
    fn cursor_shape_tracker_handles_split_decscusr_sequences() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"\x1b[");
        pane.process_pty_bytes(pane_id, 0, b"5 ");
        pane.process_pty_bytes(pane_id, 0, b"q");

        assert_eq!(
            pane.cursor_state().expect("test precondition").shape,
            crate::protocol::CursorShapeParam::BlinkingBar
        );
    }

    #[test]
    fn cursor_state_reports_the_live_position() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"x");
        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[6;21H");

        assert_eq!(result.render_delay, None);
        assert_eq!(
            pane.cursor_state()
                .map(|cursor| (cursor.x, cursor.y, cursor.visible)),
            Some((20, 5, true))
        );
    }

    #[test]
    fn cursor_state_returns_terminal_default_after_ris() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"\x1b[4 q");
        assert_eq!(
            pane.cursor_state().expect("test precondition").shape,
            crate::protocol::CursorShapeParam::SteadyUnderline
        );
        pane.process_pty_bytes(pane_id, 0, b"\x1bc");

        assert_eq!(
            pane.cursor_state().expect("test precondition").shape,
            crate::protocol::CursorShapeParam::Default
        );
    }

    /// The host theme is applied to the core directly, never written through
    /// the child's parser: a CSI the child is halfway through must survive.
    #[test]
    fn host_theme_change_does_not_split_a_partial_child_sequence() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"\x1b[3");
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: None,
            ..Default::default()
        });
        pane.process_pty_bytes(pane_id, 0, b"1mred");

        assert_eq!(pane.visible_text(), "red\n");
    }

    /// A render that force-ends a timed-out synchronized update must not
    /// lose the frame's effects: the flush entry point hands them over, and
    /// a clipboard write is no longer thrown away by the next read.
    #[test]
    fn timed_out_synchronized_update_effects_survive_a_render_flush() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let begin = pane.process_pty_bytes(
            pane_id,
            0,
            b"\x1b[?2026h\x1b]52;c;aGk=\x07\x1b]2;framed\x07\x1b[6n",
        );
        assert!(begin.terminal_responses.is_empty());
        assert!(begin.clipboard_writes.is_empty());
        let deadline = crate::ghostty::lock_terminal_core(&pane.core)
            .expect("test precondition")
            .terminal
            .synchronized_output_deadline()
            .expect("test precondition");
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        // A render flushes the frame; nothing reads its effects yet.
        assert!(matches!(
            pane.collect_dirty_patch(20, 5),
            TerminalDirtyPatchOutcome::Patch(_) | TerminalDirtyPatchOutcome::Clean
        ));
        // A resize in between must not take the queued reply with it.
        assert!(
            pane.resize(crate::geometry::PaneGeometry::new(20, 5, 0, 0))
                .is_empty()
        );

        let flushed = pane.flush_expired_synchronized_output(pane_id, 0);
        assert!(!flushed.request_render, "the render already flushed it");
        assert_eq!(
            flushed.terminal_responses,
            vec![Bytes::from_static(b"\x1b[1;1R")]
        );
        assert_eq!(flushed.clipboard_writes, vec![b"hi".to_vec()]);
        assert!(flushed.terminal_title_changed);
        assert_eq!(pane.terminal_title().as_deref(), Some("framed"));

        let next = pane.process_pty_bytes(pane_id, 0, b"x");
        assert!(next.terminal_responses.is_empty());
        assert!(next.clipboard_writes.is_empty());
    }

    #[test]
    fn flush_entry_point_ends_an_expired_update_itself() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let begin = pane.process_pty_bytes(pane_id, 0, b"\x1b[?2026h\x1b[5n");
        assert!(begin.render_delay.is_some());
        assert!(
            !pane
                .flush_expired_synchronized_output(pane_id, 0)
                .request_render
        );
        let deadline = crate::ghostty::lock_terminal_core(&pane.core)
            .expect("test precondition")
            .terminal
            .synchronized_output_deadline()
            .expect("test precondition");
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        let flushed = pane.flush_expired_synchronized_output(pane_id, 0);
        assert!(flushed.request_render);
        assert_eq!(
            flushed.terminal_responses,
            vec![Bytes::from_static(b"\x1b[0n")]
        );
        assert!(!pane.synchronized_output_state().0);
    }

    #[test]
    fn host_terminal_theme_restore_probe_skips_when_no_transient_override() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");

        assert!(!should_probe_host_terminal_theme_restore(&core));
    }

    #[test]
    fn host_terminal_theme_restore_probe_skips_when_host_theme_unknown() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.transient_default_color_owner_pgid = Some(42);
        }
        let core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");

        assert!(!should_probe_host_terminal_theme_restore(&core));
    }

    #[test]
    fn host_terminal_theme_restore_probe_skips_on_alternate_screen() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[?1049h");
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.transient_default_color_owner_pgid = Some(42);
            core.host_terminal_theme = crate::host_term::theme::TerminalTheme {
                foreground: Some(crate::host_term::theme::RgbColor {
                    r: 0xaa,
                    g: 0xbb,
                    b: 0xcc,
                }),
                background: Some(crate::host_term::theme::RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }),
                ..Default::default()
            };
        }
        let core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");

        assert!(!should_probe_host_terminal_theme_restore(&core));
    }

    #[test]
    fn host_terminal_theme_restore_probe_runs_when_restore_is_pending() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.transient_default_color_owner_pgid = Some(42);
            core.host_terminal_theme = crate::host_term::theme::TerminalTheme {
                foreground: Some(crate::host_term::theme::RgbColor {
                    r: 0xaa,
                    g: 0xbb,
                    b: 0xcc,
                }),
                background: Some(crate::host_term::theme::RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }),
                ..Default::default()
            };
        }
        let core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");

        assert!(should_probe_host_terminal_theme_restore(&core));
    }

    #[test]
    fn ghostty_render_can_suppress_cursor_position() {
        let mut first_terminal = crate::ghostty::Terminal::new(20, 5, 0);
        first_terminal.write(b"left");
        let first = GhosttyPaneTerminal::new(first_terminal);

        let mut second_terminal = crate::ghostty::Terminal::new(20, 5, 0);
        second_terminal.write(b"r\r\nb");
        let second = GhosttyPaneTerminal::new(second_terminal);

        let backend = ratatui::backend::TestBackend::new(40, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| {
                first.render(frame, Rect::new(0, 0, 20, 5), true);
                second.render(frame, Rect::new(20, 0, 20, 5), false);
            })
            .expect("test precondition");

        terminal.backend_mut().assert_cursor_position((4, 0));
    }

    #[test]
    fn ghostty_keyboard_protocol_tracks_live_terminal_flags() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[>3u");
        let pane = GhosttyPaneTerminal::new(terminal);

        assert_eq!(
            pane.keyboard_protocol(),
            Some(crate::input::KeyboardProtocol::Kitty { flags: 3 })
        );
    }

    #[test]
    fn ghostty_plain_text_chars_still_encode_as_text() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::empty(),
            ),
            crate::input::KeyboardProtocol::Legacy,
        );

        assert_eq!(encoded, b"a");
    }

    #[test]
    fn ghostty_backtab_preserves_shift_across_keyboard_protocols() {
        for (kitty_flags, expected) in [
            (None, b"\x1b[Z".as_slice()),
            (Some(1), b"\x1b[9;2u".as_slice()),
        ] {
            let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
            if let Some(flags) = kitty_flags {
                terminal.write(format!("\x1b[>{flags}u").as_bytes());
            }
            let pane = GhosttyPaneTerminal::new(terminal);
            let protocol = pane.keyboard_protocol().expect("test precondition");

            for modifiers in [
                crossterm::event::KeyModifiers::empty(),
                crossterm::event::KeyModifiers::SHIFT,
            ] {
                let encoded = pane.encode_terminal_key(
                    crate::input::TerminalKey::new(crossterm::event::KeyCode::BackTab, modifiers),
                    protocol,
                );
                assert_eq!(encoded, expected, "backtab with modifiers {modifiers:?}");
            }
        }

        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let encoded = pane.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Tab,
                crossterm::event::KeyModifiers::empty(),
            ),
            crate::input::KeyboardProtocol::Legacy,
        );
        assert_eq!(encoded, b"\t");
    }

    #[test]
    fn ghostty_ctrl_tab_matches_the_pane_keyboard_protocol() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let legacy = GhosttyPaneTerminal::new(terminal);
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::CONTROL,
        );

        assert_eq!(
            legacy.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy),
            b"\t"
        );

        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[>3u");
        let kitty = GhosttyPaneTerminal::new(terminal);
        // Flags 3 include REPORT_EVENT_TYPES; shepr's encoder always spells out
        // the press event type (`:1`), which the protocol allows.
        assert_eq!(
            kitty.encode_terminal_key(key, crate::input::KeyboardProtocol::Kitty { flags: 3 }),
            b"\x1b[9;5:1u"
        );
    }

    #[test]
    fn ghostty_legacy_modified_enter_is_shell_compatible() {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let protocol = crate::input::KeyboardProtocol::Legacy;

        for modifiers in [
            KeyModifiers::empty(),
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
            KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
        ] {
            let key = crate::input::TerminalKey::new(KeyCode::Enter, modifiers);
            let expected = if modifiers.contains(KeyModifiers::ALT) {
                b"\x1b\r".as_slice()
            } else {
                b"\r".as_slice()
            };
            for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
                assert_eq!(
                    pane.encode_terminal_key(key.clone().with_kind(kind), protocol),
                    expected,
                    "{modifiers:?} {kind:?}"
                );
            }
            assert_eq!(
                pane.encode_terminal_key(key.clone().with_repeat_count(3), protocol),
                expected.repeat(3),
                "{modifiers:?} grouped repeat"
            );
            assert!(
                pane.encode_terminal_key(key.with_kind(KeyEventKind::Release), protocol)
                    .is_empty(),
                "{modifiers:?} release"
            );
        }
    }

    #[test]
    fn ghostty_modified_enter_tracks_live_protocol_negotiation() {
        use crossterm::event::{KeyCode, KeyModifiers};

        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        let legacy = ["\r", "\r", "\r", "\x1b\r"];
        let mode_one = ["\x1b[27;2;13~", "\x1b[27;5;13~", "\x1b[27;9;13~", "\x1b\r"];
        let mode_two = [
            "\x1b[27;2;13~",
            "\x1b[27;5;13~",
            "\x1b[27;9;13~",
            "\x1b[27;3;13~",
        ];
        let kitty = ["\x1b[13;2u", "\x1b[13;5u", "\x1b[13;9u", "\x1b[13;3u"];

        for (sequence, expected) in [
            ("", legacy),
            ("\x1b[>4;1m", mode_one),
            ("\x1b[>4;2m", mode_two),
            ("\x1b[>4n", legacy),
            ("\x1b[>4;2m", mode_two),
            ("\x1b[>4;0m", legacy),
            ("\x1b[>5u", kitty),
            ("\x1b[<u", legacy),
            ("\x1b[>4;2m\x1b[>1u", kitty),
            ("\x1b[<u", mode_two),
            ("\x1b[>4;0m", legacy),
            ("\x1b[>4;1m", mode_one),
            ("\x1b[>04n", legacy),
            ("\x1b[>4;2m", mode_two),
            ("\x1b[>4", mode_two),
            ("n", legacy),
        ] {
            pane.process_pty_bytes(pane_id, 0, sequence.as_bytes());
            for (modifiers, expected) in [
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
                KeyModifiers::SUPER,
                KeyModifiers::ALT,
            ]
            .into_iter()
            .zip(expected)
            {
                let key = crate::input::TerminalKey::new(KeyCode::Enter, modifiers);
                assert_eq!(
                    pane.encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
                    expected.as_bytes(),
                    "{modifiers:?} after {sequence:?}"
                );
            }
        }
    }

    #[test]
    fn ghostty_modified_enter_respects_existing_terminal_mode() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[>4;2m");
        let pane = GhosttyPaneTerminal::new(terminal);
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::SHIFT,
        );

        assert_eq!(
            pane.encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
            b"\x1b[27;2;13~"
        );
    }

    #[test]
    fn ghostty_enter_backspace_release_in_legacy_pane_emits_nothing() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);

        for code in [
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyCode::Backspace,
        ] {
            let press = pane.encode_terminal_key(
                crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty()),
                crate::input::KeyboardProtocol::Legacy,
            );
            let release = pane.encode_terminal_key(
                crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty())
                    .with_kind(crossterm::event::KeyEventKind::Release),
                crate::input::KeyboardProtocol::Legacy,
            );
            assert!(!press.is_empty(), "{code:?} press should emit bytes");
            assert!(
                release.is_empty(),
                "{code:?} release should emit nothing in a legacy pane, got {release:?}"
            );
        }
    }

    #[test]
    fn ghostty_report_event_pane_keeps_basic_compatibility_keys_legacy() {
        // Push kitty flags including REPORT_EVENT_TYPES (0b10) + DISAMBIGUATE (0b1).
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[>3u");
        let pane = GhosttyPaneTerminal::new(terminal);

        for (code, expected) in [
            (crossterm::event::KeyCode::Enter, b"\r".as_slice()),
            (crossterm::event::KeyCode::Backspace, b"\x7f".as_slice()),
        ] {
            let press = pane.encode_terminal_key(
                crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty()),
                pane.keyboard_protocol().expect("test precondition"),
            );
            assert_eq!(
                press, expected,
                "{code:?} press should stay legacy-compatible without REPORT_ALL_KEYS"
            );

            let release = pane.encode_terminal_key(
                crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::empty())
                    .with_kind(crossterm::event::KeyEventKind::Release),
                pane.keyboard_protocol().expect("test precondition"),
            );
            assert!(
                release.is_empty(),
                "{code:?} release should not fall back to legacy bytes, got {release:?}"
            );
        }
    }

    #[test]
    fn ghostty_char_keys_still_use_shepr_encoding() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[>1u");
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
            ),
            crate::input::KeyboardProtocol::Legacy,
        );

        assert_eq!(encoded, vec![1]);
    }

    #[test]
    fn ghostty_key_encoding_honors_application_cursor_mode() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal
            .mode_set(crate::ghostty::MODE_APPLICATION_CURSOR_KEYS, true)
            .expect("test precondition");
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Up,
                crossterm::event::KeyModifiers::empty(),
            ),
            crate::input::KeyboardProtocol::Legacy,
        );

        assert_eq!(encoded, b"\x1bOA");
    }

    #[test]
    fn grouped_key_repeats_expand_at_the_destination() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::empty(),
        )
        .with_repeat_count(3);

        assert_eq!(
            pane.encode_terminal_key(key, crate::input::KeyboardProtocol::Legacy),
            b"xxx"
        );

        let shifted = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::SHIFT,
        )
        .with_generated_text(Some("/".to_owned()))
        .with_repeat_count(3);
        let legacy_expected = b"///".as_slice();
        assert_eq!(
            pane.encode_terminal_key(shifted.clone(), crate::input::KeyboardProtocol::Legacy,),
            legacy_expected
        );
        // Flags 15 (disambiguate + event types + alternate keys + report all
        // keys) reports every key as CSI u but, without flag 16
        // (REPORT_ASSOCIATED_TEXT), carries no committed text: the repeat
        // still has to expand to three identical CSI u sequences rather than
        // three literal slashes.
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[>15u");
        let pane = GhosttyPaneTerminal::new(terminal);
        let kitty_protocol = crate::input::KeyboardProtocol::Kitty { flags: 15 };
        let pressed =
            pane.encode_terminal_key_once(shifted.clone().with_repeat_count(1), kitty_protocol);
        assert!(
            !pressed.is_empty() && pressed != b"/",
            "flags 15 without REPORT_ASSOCIATED_TEXT should encode as CSI u, not plain text"
        );
        let repeated_key = shifted
            .clone()
            .with_repeat_count(1)
            .with_kind(crossterm::event::KeyEventKind::Repeat);
        let repeated = pane.encode_terminal_key_once(repeated_key, kitty_protocol);
        let mut expected = pressed;
        expected.extend_from_slice(&repeated);
        expected.extend_from_slice(&repeated);
        assert_eq!(pane.encode_terminal_key(shifted, kitty_protocol), expected);
    }

    #[test]
    fn grouped_release_is_encoded_once() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[>11u");
        let pane = GhosttyPaneTerminal::new(terminal);
        let protocol = pane.keyboard_protocol().expect("test precondition");
        let release = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::empty(),
        )
        .with_kind(crossterm::event::KeyEventKind::Release);
        let expected = pane.encode_terminal_key(release.clone(), protocol);

        assert!(!expected.is_empty());
        let mut malformed_release = release;
        malformed_release.repeat_count = 3;
        assert_eq!(
            pane.encode_terminal_key(malformed_release, protocol),
            expected
        );
    }

    #[test]
    fn ghostty_key_encoder_updates_after_terminal_mode_changes() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let before = pane.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Up,
                crossterm::event::KeyModifiers::empty(),
            ),
            crate::input::KeyboardProtocol::Legacy,
        );
        assert_eq!(before, b"\x1b[A");

        pane.process_pty_bytes(pane_id, 0, b"\x1b[?1h");

        let after = pane.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Up,
                crossterm::event::KeyModifiers::empty(),
            ),
            crate::input::KeyboardProtocol::Legacy,
        );
        assert_eq!(after, b"\x1bOA");
    }

    #[test]
    fn ghostty_key_encoder_updates_after_kitty_flag_changes() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        );

        let before = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);
        pane.process_pty_bytes(pane_id, 0, b"\x1b[>1u");
        let after = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

        assert_ne!(before, after);
        assert_eq!(after, b"\x1b[13;6u");
    }

    #[test]
    fn ghostty_kitty_pane_encodes_shift_enter_as_csi_u() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.process_pty_bytes(pane_id, 0, b"\x1b[>5u");

        let key =
            crate::input::parse_terminal_key_sequence("\x1b[13;2u").expect("test precondition");
        let encoded = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

        assert_eq!(
            pane.keyboard_protocol(),
            Some(crate::input::KeyboardProtocol::Kitty { flags: 5 })
        );
        assert_eq!(encoded, b"\x1b[13;2u");
    }

    #[test]
    fn ghostty_modify_other_keys_mode_one_preserves_shift_enter() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let key =
            crate::input::parse_terminal_key_sequence("\x1b[13;2u").expect("test precondition");

        pane.seed_history_ansi("\x1b[>4;1m");
        assert_eq!(pane.modify_other_keys_level(), 1);
        let encoded = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

        assert_eq!(encoded, b"\x1b[27;2;13~");
    }

    #[test]
    fn ghostty_kitty_pane_encodes_parsed_legacy_alt_backspace_as_csi_u() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.process_pty_bytes(pane_id, 0, b"\x1b[>1u");

        let key = crate::input::parse_terminal_key_sequence("\x1b\x7f").expect("test precondition");
        let encoded = pane.encode_terminal_key(key.clone(), crate::input::KeyboardProtocol::Legacy);

        assert_eq!(encoded, b"\x1b[127;3u");
    }

    #[test]
    fn ghostty_kitty_pane_preserves_legacy_ctrl_alt_letter() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.process_pty_bytes(pane_id, 0, b"\x1b[>5u");

        let mut events = crate::raw_input::parse_raw_input_bytes_sync(b"\x1b\x06");
        let crate::raw_input::RawInputEvent::Key(key) = events.remove(0) else {
            panic!("expected key event");
        };
        let encoded =
            pane.encode_terminal_key(key, pane.keyboard_protocol().expect("test precondition"));

        assert_eq!(encoded, b"\x1b[102;7u");
    }

    #[test]
    fn ghostty_pane_characterizes_ctrl_backspace_encoding() {
        let legacy = GhosttyPaneTerminal::new(crate::ghostty::Terminal::new(80, 24, 0));

        let ctrl_backspace = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(
            legacy.encode_terminal_key(
                ctrl_backspace.clone(),
                crate::input::KeyboardProtocol::Legacy
            ),
            b"\x08"
        );

        let plain_backspace = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::empty(),
        );
        assert_eq!(
            legacy.encode_terminal_key(plain_backspace, crate::input::KeyboardProtocol::Legacy),
            b"\x7f"
        );

        let kitty = GhosttyPaneTerminal::new(crate::ghostty::Terminal::new(80, 24, 0));
        let pane_id = PaneId::from_raw(1);
        kitty.process_pty_bytes(pane_id, 0, b"\x1b[>1u");

        assert_eq!(
            kitty.encode_terminal_key(ctrl_backspace, crate::input::KeyboardProtocol::Legacy),
            b"\x1b[127;5u"
        );
    }

    #[test]
    fn ghostty_key_encoders_are_isolated_per_pane() {
        let first = GhosttyPaneTerminal::new(crate::ghostty::Terminal::new(80, 24, 0));
        let second = GhosttyPaneTerminal::new(crate::ghostty::Terminal::new(80, 24, 0));

        first.process_pty_bytes(PaneId::from_raw(1), 0, b"\x1b[?1h");

        let first_encoded = first.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Up,
                crossterm::event::KeyModifiers::empty(),
            ),
            crate::input::KeyboardProtocol::Legacy,
        );
        let second_encoded = second.encode_terminal_key(
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Up,
                crossterm::event::KeyModifiers::empty(),
            ),
            crate::input::KeyboardProtocol::Legacy,
        );

        assert_eq!(first_encoded, b"\x1bOA");
        assert_eq!(second_encoded, b"\x1b[A");
    }

    #[test]
    fn ghostty_mouse_button_encoding_uses_live_terminal_state() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[?1000h\x1b[?1006h");
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_mouse_button(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            crate::input::mouse::Position::Cell { column: 11, row: 9 },
            crossterm::event::KeyModifiers::empty(),
        );

        assert_eq!(encoded.as_deref(), Some(&b"\x1b[<0;12;10m"[..]));
    }

    #[test]
    fn ghostty_mouse_drag_encoding_uses_motion_reporting_state() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[?1002h\x1b[?1006h");
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_mouse_button(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            crate::input::mouse::Position::Cell { column: 4, row: 6 },
            crossterm::event::KeyModifiers::SHIFT,
        );

        assert_eq!(encoded.as_deref(), Some(&b"\x1b[<36;5;7M"[..]));
    }

    #[test]
    fn ghostty_mouse_drag_without_motion_reporting_is_not_forwarded() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[?1000h\x1b[?1006h");
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_mouse_button(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            crate::input::mouse::Position::Cell { column: 4, row: 6 },
            crossterm::event::KeyModifiers::empty(),
        );

        assert_eq!(encoded, None);
    }

    #[test]
    fn ghostty_mouse_moved_encoding_uses_any_motion_state() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.write(b"\x1b[?1003h\x1b[?1006h");
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_mouse_motion(
            crossterm::event::MouseEventKind::Moved,
            crate::input::mouse::Position::Cell { column: 4, row: 6 },
            crossterm::event::KeyModifiers::empty(),
        );

        assert_eq!(encoded.as_deref(), Some(&b"\x1b[<35;5;7M"[..]));
    }

    #[test]
    fn ghostty_mouse_sgr_pixels_preserves_exact_and_maps_cell_input_to_pixels() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.resize(crate::geometry::PaneGeometry::new(80, 24, 10, 20));
        terminal.write(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        let pane = GhosttyPaneTerminal::new(terminal);

        let exact = pane.encode_mouse_motion(
            crossterm::event::MouseEventKind::Moved,
            crate::input::mouse::Position::Pixels { x: 48, y: 139 },
            crossterm::event::KeyModifiers::empty(),
        );
        let from_cell = pane.encode_mouse_motion(
            crossterm::event::MouseEventKind::Moved,
            crate::input::mouse::Position::Cell { column: 4, row: 6 },
            crossterm::event::KeyModifiers::empty(),
        );

        assert_eq!(exact.as_deref(), Some(&b"\x1b[<35;48;139M"[..]));
        // Column 4 at 10 px per cell starts at pixel 41; row 6 at 20 px at 121.
        assert_eq!(from_cell.as_deref(), Some(&b"\x1b[<35;41;121M"[..]));
    }

    #[test]
    fn ghostty_mouse_sgr_pixels_without_pixel_geometry_sends_cells() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.resize(crate::geometry::PaneGeometry::new(80, 24, 0, 0));
        terminal.write(b"\x1b[?1003h\x1b[?1006h\x1b[?1016h");
        let pane = GhosttyPaneTerminal::new(terminal);

        let encoded = pane.encode_mouse_motion(
            crossterm::event::MouseEventKind::Moved,
            crate::input::mouse::Position::Cell { column: 4, row: 6 },
            crossterm::event::KeyModifiers::empty(),
        );

        assert_eq!(encoded.as_deref(), Some(&b"\x1b[<35;5;7M"[..]));
    }

    #[test]
    fn ghostty_normalize_buffer_symbol_prefers_grapheme_width_when_metadata_disagrees() {
        const WIDE_GRAPHEME: &str = "\u{1F642}";
        const FLAG_GRAPHEME: &str = "\u{1F1E7}\u{1F1F7}";
        const FAMILY_GRAPHEME: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        const VS16_GRAPHEME: &str = "\u{26A0}\u{FE0F}";
        const EMOJI_GRAPHEME: &str = "\u{1F4B3}";

        assert_eq!(
            ghostty_normalize_buffer_symbol(WIDE_GRAPHEME, crate::ghostty::CellWide::Wide),
            WIDE_GRAPHEME
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol("a", crate::ghostty::CellWide::Wide),
            "  "
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol(FLAG_GRAPHEME, crate::ghostty::CellWide::Wide),
            FLAG_GRAPHEME
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol(FAMILY_GRAPHEME, crate::ghostty::CellWide::Wide),
            FAMILY_GRAPHEME
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol("⌨\u{FE0F}", crate::ghostty::CellWide::Narrow),
            "⌨\u{FE0F}"
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol(VS16_GRAPHEME, crate::ghostty::CellWide::Narrow),
            VS16_GRAPHEME
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol(EMOJI_GRAPHEME, crate::ghostty::CellWide::Narrow),
            EMOJI_GRAPHEME
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol(" ", crate::ghostty::CellWide::SpacerTail),
            ""
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol("xx", crate::ghostty::CellWide::SpacerHead),
            " "
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol("ｶ\u{ff9e}", crate::ghostty::CellWide::Wide),
            "ｶ\u{ff9e}"
        );
        assert_eq!(
            ghostty_normalize_buffer_symbol("ﾊ\u{ff9f}", crate::ghostty::CellWide::Wide),
            "ﾊ\u{ff9f}"
        );
    }

    fn render_cells_to_symbols(
        terminal: &mut crate::ghostty::Terminal,
    ) -> Vec<(crate::ghostty::CellWide, String)> {
        let mut render_state = crate::ghostty::RenderState::new();
        render_state.update(terminal);

        let mut symbol_scratch = String::new();
        let mut out = Vec::new();

        if let Some(row) = render_state.iter_rows().next() {
            for cells in row.cells() {
                let wide = cells.wide();
                let symbol = ghostty_buffer_symbol_into(&cells, wide, false, &mut symbol_scratch)
                    .to_string();
                out.push((wide, symbol));
            }
        }

        out
    }

    // The core lays out one codepoint per cell group (no grapheme clustering):
    // zero-width joiners and selectors attach to the preceding cell, so the
    // rendered cells still spell out the original text in order.
    #[test]
    fn multi_codepoint_emoji_render_without_losing_text() {
        for text in [
            "\u{1F1E7}\u{1F1F7}",
            "\u{1F468}\u{200d}\u{1F469}\u{200d}\u{1F467}",
            "\u{26A0}\u{fe0f}",
        ] {
            let mut terminal = crate::ghostty::Terminal::new(40, 1, 0);
            terminal.write(text.as_bytes());

            let cells = render_cells_to_symbols(&mut terminal);
            let rendered: String = cells.iter().map(|(_, symbol)| symbol.as_str()).collect();

            assert_eq!(rendered.trim_end(), text, "{cells:?}");
        }
    }

    #[test]
    fn halfwidth_katakana_voiced_marks_render() {
        let mut terminal = crate::ghostty::Terminal::new(40, 1, 0);
        terminal.write("ｱｲｳｴｵ ｶﾞｷﾞｸﾞｹﾞｺﾞ ﾊﾟﾋﾟﾌﾟﾍﾟﾎﾟ".as_bytes());

        let cells = render_cells_to_symbols(&mut terminal);
        let rendered: String = cells.iter().map(|(_, symbol)| symbol.as_str()).collect();

        assert!(
            rendered.contains("ｱｲｳｴｵ ｶﾞｷﾞｸﾞｹﾞｺﾞ ﾊﾟﾋﾟﾌﾟﾍﾟﾎﾟ"),
            "expected halfwidth katakana with voiced marks to survive, got {cells:?}"
        );
    }

    #[test]
    fn render_keeps_halfwidth_katakana_and_voiced_mark_in_their_own_cells() {
        let mut terminal = crate::ghostty::Terminal::new(20, 1, 0);
        terminal.write("ｶﾞZ".as_bytes());
        let pane = GhosttyPaneTerminal::new(terminal);

        let backend = ratatui::backend::TestBackend::new(20, 1);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 1), false))
            .expect("test precondition");
        let buffer = terminal.backend().buffer();

        // The halfwidth voiced mark is a spacing character one cell wide.
        assert_eq!(buffer[(0, 0)].symbol(), "ｶ");
        assert_eq!(buffer[(1, 0)].symbol(), "\u{ff9e}");
        assert_eq!(buffer[(2, 0)].symbol(), "Z");
    }

    #[test]
    fn pane_scrollback_controls_round_trip_and_clamp_without_ui_interference() {
        let mut terminal = crate::ghostty::Terminal::new(80, 3, 100);
        write_numbered_lines(&mut terminal, 1000);
        let pane = GhosttyPaneTerminal::new(terminal);

        let before = pane.scroll_metrics().expect("scroll metrics before scroll");
        assert!(before.max_offset_from_bottom > 0);
        assert_eq!(before.offset_from_bottom, 0);

        for offset in [
            0,
            before.max_offset_from_bottom / 2,
            before.max_offset_from_bottom,
            usize::MAX,
        ] {
            pane.set_scroll_offset_from_bottom(offset);
            let after = pane.scroll_metrics().expect("scroll metrics after scroll");
            assert_eq!(
                after.offset_from_bottom,
                offset.min(after.max_offset_from_bottom)
            );
        }

        assert!(pane.visible_text().contains("000000"));
    }

    #[test]
    fn empty_or_short_resize_keeps_following_bottom_when_output_creates_scrollback() {
        for initial in [b"".as_slice(), b"seed\r\n".as_slice()] {
            let mut terminal = crate::ghostty::Terminal::new(10, 3, 100);
            terminal.write(initial);
            let pane = GhosttyPaneTerminal::new(terminal);
            let pane_id = PaneId::from_raw(1);

            pane.resize(crate::geometry::PaneGeometry::new(10, 3, 0, 0));
            pane.process_pty_bytes(
                pane_id,
                0,
                b"000000\r\n000001\r\n000002\r\n000003\r\n000004",
            );

            let metrics = pane.scroll_metrics().expect("scroll metrics after output");
            assert_eq!(metrics.offset_from_bottom, 0);
            assert!(pane.visible_text().contains("000004"));
        }
    }

    #[test]
    fn resize_that_removes_scrollback_restores_live_follow() {
        let mut terminal = crate::ghostty::Terminal::new(10, 3, 100);
        terminal.write(b"000000\r\n000001\r\n000002\r\n000003\r\n000004");
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.set_scroll_offset_from_bottom(1);
        pane.resize(crate::geometry::PaneGeometry::new(10, 5, 0, 0));
        let resized = pane.scroll_metrics().expect("scroll metrics after resize");
        assert_eq!(resized.max_offset_from_bottom, 0);

        pane.process_pty_bytes(pane_id, 0, b"\r\n000005\r\n000006");

        let metrics = pane.scroll_metrics().expect("scroll metrics after output");
        assert_eq!(metrics.offset_from_bottom, 0);
        assert!(pane.visible_text().contains("000006"));
    }

    #[test]
    fn detection_text_stays_at_bottom_when_viewport_is_scrolled() {
        let mut terminal = crate::ghostty::Terminal::new(80, 3, 100);
        write_numbered_lines(&mut terminal, 10);
        let pane = GhosttyPaneTerminal::new(terminal);

        let bottom_snapshot = pane.detection_text();
        assert_eq!(bottom_snapshot, pane.recent_text(3));
        assert!(bottom_snapshot.contains("000009"));

        let before = pane.scroll_metrics().expect("scroll metrics before scroll");
        pane.set_scroll_offset_from_bottom(before.max_offset_from_bottom);

        assert!(pane.visible_text().contains("000000"));
        assert_eq!(pane.detection_text(), bottom_snapshot);
    }

    #[test]
    fn extract_selection_uses_stable_rows_after_viewport_moves() {
        let mut terminal = crate::ghostty::Terminal::new(8, 3, 1024);
        write_numbered_lines(&mut terminal, 8);
        let pane = GhosttyPaneTerminal::new(terminal);

        pane.set_scroll_offset_from_bottom(3);
        let metrics = pane
            .scroll_metrics()
            .expect("scroll metrics after initial scroll");
        let mut selection = crate::selection::Selection::anchor(
            PaneId::from_raw(1),
            Point::new(metrics.absolute_row_at_viewport(ViewportRow(0)), 0),
        );
        selection.drag(Point::new(
            metrics.absolute_row_at_viewport(ViewportRow(2)),
            5,
        ));

        pane.scroll_reset();

        let text = pane
            .extract_selection(&selection)
            .expect("selection should extract text");
        assert_eq!(text, "000003\n000004\n000005");
    }

    #[test]
    fn recent_reads_include_viewport_before_scrollback_exists() {
        let mut terminal =
            crate::ghostty::Terminal::new(20, 20, crate::config::DEFAULT_SCROLLBACK_LIMIT_BYTES);
        terminal.write(b"hello123");
        let pane = GhosttyPaneTerminal::new(terminal);

        assert_eq!(pane.recent_text(3), "hello123\n");
        assert_eq!(pane.recent_unwrapped_text(3), "hello123");
    }

    #[test]
    fn alternate_screen_recent_reads_keep_physical_row_ranges() {
        let mut terminal = crate::ghostty::Terminal::new(20, 20, 100);
        terminal.write(b"\x1b[?1049hhello123");
        let pane = GhosttyPaneTerminal::new(terminal);

        assert_eq!(pane.recent_text(3), "");
        assert_eq!(pane.recent_unwrapped_text(3), "");
    }

    #[test]
    fn recent_unwrapped_text_ignores_soft_wraps() {
        let mut terminal = crate::ghostty::Terminal::new(5, 3, 100);
        terminal.write(b"ABCDEFGHIJ");
        let pane = GhosttyPaneTerminal::new(terminal);

        assert_eq!(pane.recent_text(3), "ABCDE\nFGHIJ\n");
        assert_eq!(pane.recent_unwrapped_text(3), "ABCDEFGHIJ");
    }

    #[test]
    fn recent_snapshots_report_omitted_rendered_rows() {
        let mut terminal = crate::ghostty::Terminal::new(20, 3, 100);
        terminal.write(b"one\r\ntwo\r\nthree\r\nfour");
        let pane = GhosttyPaneTerminal::new(terminal);

        assert!(pane.recent_text_snapshot(2).truncated);
        assert!(pane.recent_ansi_snapshot(2).truncated);
        assert!(pane.recent_unwrapped_text_snapshot(2).truncated);
        assert!(pane.recent_unwrapped_ansi_snapshot(2).truncated);
        assert!(!pane.recent_text_snapshot(100).truncated);
    }

    #[test]
    fn recent_snapshots_do_not_count_trailing_blank_rows_as_omitted() {
        let mut terminal = crate::ghostty::Terminal::new(20, 10, 100);
        terminal.write(b"one\r\ntwo");
        let pane = GhosttyPaneTerminal::new(terminal);

        // Ten rows exist but only two hold content; a five-row read leaves
        // nothing out above it.
        let snapshot = pane.recent_text_snapshot(5);
        assert_eq!(snapshot.text, "one\ntwo\n");
        assert!(!snapshot.truncated);
        assert!(!pane.recent_ansi_snapshot(5).truncated);
        assert!(!pane.recent_unwrapped_text_snapshot(5).truncated);
    }

    #[test]
    fn detection_text_ignores_the_frame_a_clear_pushed_into_history() {
        let mut terminal = crate::ghostty::Terminal::new(20, 4, 100);
        terminal.write(b"a\r\nb\r\nc\r\nproceed? [y/n]");
        terminal.write(b"\x1b[H\x1b[2Jfresh");
        let pane = GhosttyPaneTerminal::new(terminal);

        let detection = pane.detection_text();
        assert_eq!(detection, "fresh\n");
        assert!(!detection.contains("proceed"));
    }

    #[test]
    fn seeded_history_leaves_the_cursor_on_a_fresh_line() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 100);
        let pane = GhosttyPaneTerminal::new(terminal);
        // Saved history is trimmed and ends mid-line on the old prompt.
        pane.seed_history_ansi("output\r\nuser@host $ ");
        let cursor = pane.cursor_state().expect("test precondition");
        assert_eq!((cursor.x, cursor.y), (0, 2));

        pane.process_pty_bytes(PaneId::from_raw(1), 0, b"new $ ");
        assert_eq!(pane.recent_text(5), "output\nuser@host $\nnew $\n");
    }

    #[test]
    fn seeded_history_ending_in_a_line_break_gets_no_extra_blank_line() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 100);
        let pane = GhosttyPaneTerminal::new(terminal);
        pane.seed_history_ansi("restored\r\n");
        let cursor = pane.cursor_state().expect("test precondition");
        assert_eq!((cursor.x, cursor.y), (0, 1));
    }

    #[test]
    fn plain_text_reads_skip_wide_character_spacer_cells() {
        let mut terminal = crate::ghostty::Terminal::new(40, 3, 100);
        terminal.write("日本語テスト ABC 123".as_bytes());
        let pane = GhosttyPaneTerminal::new(terminal);

        assert_eq!(pane.visible_text(), "日本語テスト ABC 123\n");
        assert_eq!(pane.recent_text(3), "日本語テスト ABC 123\n");
        assert_eq!(pane.recent_unwrapped_text(3), "日本語テスト ABC 123");
        assert_eq!(pane.detection_text(), "日本語テスト ABC 123\n");
    }

    #[test]
    fn recent_rows_preserve_combining_text_and_hide_image_placeholders() {
        let mut terminal = crate::ghostty::Terminal::new(40, 3, 1024 * 1024);
        terminal.write("old\r\n".repeat(100).as_bytes());
        terminal.write("界 e\u{301} \u{10eeee} tail  ".as_bytes());
        let pane = GhosttyPaneTerminal::new(terminal);
        assert_eq!(pane.recent_text(1), "界 e\u{301}   tail\n");
        let detection = pane.detection_text();
        assert_eq!(detection, "old\nold\n界 e\u{301}   tail\n");
        pane.set_scroll_offset_from_bottom(100);
        assert_eq!(pane.detection_text(), detection);
    }

    #[test]
    fn visible_ansi_preserves_cell_style_sequences() {
        let mut terminal = crate::ghostty::Terminal::new(20, 3, 100);
        terminal.write(b"\x1b[31;1mred\x1b[0m plain");
        let pane = GhosttyPaneTerminal::new(terminal);

        let ansi = pane.visible_ansi();
        assert!(ansi.contains("red"));
        assert!(ansi.contains("plain"));
        assert!(ansi.contains("\x1b["));
    }

    #[test]
    fn recent_ansi_can_read_styled_scrollback() {
        let mut terminal = crate::ghostty::Terminal::new(20, 3, 100);
        terminal.write(b"\x1b[34mblue\x1b[0m\r\nline2\r\nline3\r\nline4");
        let pane = GhosttyPaneTerminal::new(terminal);

        let ansi = pane.recent_ansi(4);
        assert!(ansi.contains("blue"));
        assert!(ansi.contains("line4"));
        assert!(ansi.contains("\x1b["));
    }

    #[test]
    fn resize_shrinks_both_axes_with_cursor_at_old_bottom() {
        let mut terminal = crate::ghostty::Terminal::new(8, 4, 10_000);
        terminal.write(b"alpha\r\nbeta\r\ngamma\r\ndelta");
        let pane = GhosttyPaneTerminal::new(terminal);

        pane.resize(crate::geometry::PaneGeometry::new(7, 3, 8, 16));

        assert_eq!(pane.visible_text(), "beta\ngamma\ndelta\n");
        assert_eq!(pane.detection_text(), "beta\ngamma\ndelta\n");
        assert_eq!(
            pane.scroll_metrics(),
            Some(ScrollMetrics {
                offset_from_bottom: 0,
                max_offset_from_bottom: 1,
                viewport_rows: 3,
                history_origin: AbsRow(4),
            })
        );
    }

    #[test]
    fn resize_reflow_keeps_scrolled_viewport_and_bottom_detection_sane() {
        let mut terminal = crate::ghostty::Terminal::new(12, 4, 10_000);
        write_wrapped_contract_lines(&mut terminal, 40);
        let pane = GhosttyPaneTerminal::new(terminal);

        let bottom_snapshot = pane.detection_text();
        assert!(bottom_snapshot.contains("END"));

        let initial = pane.scroll_metrics().expect("initial scroll metrics");
        assert!(initial.max_offset_from_bottom > 0);
        pane.set_scroll_offset_from_bottom(initial.max_offset_from_bottom / 2);
        assert!(!pane.visible_text().trim().is_empty());

        for (rows, cols) in [(4, 10), (4, 7), (6, 18), (3, 9), (5, 12)] {
            let before_resize = pane.scroll_metrics().expect("scroll metrics before resize");
            pane.resize(crate::geometry::PaneGeometry::new(cols, rows, 0, 0));

            let metrics = pane.scroll_metrics().expect("scroll metrics after resize");
            assert_eq!(metrics.viewport_rows, rows as usize);
            assert_eq!(
                metrics.offset_from_bottom,
                before_resize
                    .offset_from_bottom
                    .min(metrics.max_offset_from_bottom)
            );
            assert!(
                metrics.offset_from_bottom > 0,
                "resize should preserve a scrolled viewport instead of jumping to bottom"
            );
            assert!(metrics.max_offset_from_bottom > 0);
            let visible = pane.visible_text();
            assert!(
                !visible.trim().is_empty(),
                "visible text should not be empty after resize to {rows}x{cols}; metrics={metrics:?}; detection={:?}; recent={:?}",
                pane.detection_text(),
                pane.recent_text(6)
            );
            assert!(
                pane.detection_text().contains("END"),
                "bottom detection should remain independent from the scrolled viewport after resize"
            );
        }
    }

    #[test]
    fn resize_recovery_does_not_replay_history_when_visible_screen_was_blank() {
        let mut terminal = crate::ghostty::Terminal::new(20, 3, 10_000);
        terminal.write(b"old history\r\n\x1b[2J\x1b[H");
        let pane = GhosttyPaneTerminal::new(terminal);

        assert!(pane.visible_text().trim().is_empty());
        assert!(pane.detection_text().trim().is_empty());

        pane.resize(crate::geometry::PaneGeometry::new(20, 3, 0, 0));

        assert!(pane.visible_text().trim().is_empty());
        assert!(pane.detection_text().trim().is_empty());
        assert!(pane.recent_text(3).trim().is_empty());
    }

    #[test]
    fn resize_recovery_does_not_replay_scrolled_history_over_blank_bottom() {
        let mut terminal = crate::ghostty::Terminal::new(20, 3, 10_000);
        write_numbered_lines(&mut terminal, 20);
        terminal.write(b"\x1b[2J\x1b[H");
        let pane = GhosttyPaneTerminal::new(terminal);

        assert!(pane.detection_text().trim().is_empty());
        let metrics = pane.scroll_metrics().expect("scroll metrics");
        pane.set_scroll_offset_from_bottom(metrics.max_offset_from_bottom);
        assert!(!pane.visible_text().trim().is_empty());

        pane.resize(crate::geometry::PaneGeometry::new(20, 3, 0, 0));

        assert!(pane.detection_text().trim().is_empty());
        assert!(pane.recent_text(3).trim().is_empty());
    }

    #[test]
    fn process_pty_bytes_answers_xtwinops_size_queries() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.resize(crate::geometry::PaneGeometry::new(80, 24, 9, 18));

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[14t\x1b[16t\x1b[18t");

        assert_eq!(
            result.terminal_responses,
            vec![
                Bytes::from_static(b"\x1b[4;432;720t"),
                Bytes::from_static(b"\x1b[6;18;9t"),
                Bytes::from_static(b"\x1b[8;24;80t"),
            ]
        );
    }

    #[test]
    fn xtwinops_size_queries_follow_successful_resize() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.resize(crate::geometry::PaneGeometry::new(80, 24, 9, 18));
        pane.resize(crate::geometry::PaneGeometry::new(100, 30, 10, 20));

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[14t\x1b[16t\x1b[18t");

        assert_eq!(
            result.terminal_responses,
            vec![
                Bytes::from_static(b"\x1b[4;600;1000t"),
                Bytes::from_static(b"\x1b[6;20;10t"),
                Bytes::from_static(b"\x1b[8;30;100t"),
            ]
        );
    }

    #[test]
    fn xtwinops_size_queries_stay_silent_without_pixel_geometry() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        for (cell_width_px, cell_height_px) in [(0, 0), (0, 18), (9, 0)] {
            pane.resize(crate::geometry::PaneGeometry::new(
                80,
                24,
                cell_width_px,
                cell_height_px,
            ));
            let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[14t\x1b[16t\x1b[18t");
            // CSI 14 t (pixel geometry) and CSI 16 t (cell size in pixels) stay
            // silent without pixel geometry, but CSI 18 t reports characters,
            // which is always known, so it is answered regardless.
            assert_eq!(
                result.terminal_responses,
                vec![Bytes::from_static(b"\x1b[8;24;80t")]
            );
        }
    }

    #[test]
    fn enabling_in_band_size_reports_after_alt_screen_resize_reports_current_size() {
        let terminal = crate::ghostty::Terminal::new(91, 24, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049h");
        assert!(
            pane.resize(crate::geometry::PaneGeometry::new(92, 24, 9, 18))
                .is_empty()
        );

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[?2048h");

        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b[48;24;92;432;828t")]
        );
    }

    #[test]
    fn resize_returns_in_band_size_report_response() {
        let mut terminal = crate::ghostty::Terminal::new(80, 24, 0);
        terminal.mode_set(2048, true).expect("test precondition");
        let pane = GhosttyPaneTerminal::new(terminal);

        let responses = pane.resize(crate::geometry::PaneGeometry::new(100, 40, 9, 18));

        assert_eq!(
            responses,
            vec![Bytes::from_static(b"\x1B[48;40;100;720;900t")]
        );
    }

    #[test]
    fn synchronized_output_suppresses_intermediate_render_requests_until_batch_ends() {
        let terminal = crate::ghostty::Terminal::new(80, 24, 0);
        let pane_terminal = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        assert_eq!(pane_terminal.synchronized_output_state(), (false, 0));
        pane_terminal.process_pty_bytes(pane_id, 0, b"ordinary output");
        assert_eq!(pane_terminal.synchronized_output_state(), (false, 0));

        let begin = pane_terminal.process_pty_bytes(pane_id, 0, b"\x1b[?2026h");
        assert!(!begin.request_render);
        assert_eq!(pane_terminal.synchronized_output_state(), (true, 1));

        let body = pane_terminal.process_pty_bytes(pane_id, 0, b"hello");
        assert!(!body.request_render);
        assert_eq!(pane_terminal.synchronized_output_state(), (true, 1));

        let end = pane_terminal.process_pty_bytes(pane_id, 0, b"\x1b[?2026l");
        assert!(end.request_render);
        assert_eq!(pane_terminal.synchronized_output_state(), (false, 2));
    }

    #[test]
    fn seeded_history_is_rendered_on_next_draw() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 100);
        let pane = GhosttyPaneTerminal::new(terminal);
        pane.seed_history_ansi("restored history");

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        let row = (0..16).map(|x| buffer[(x, 0)].symbol()).collect::<String>();
        assert_eq!(row, "restored history");
    }

    #[test]
    fn render_leaves_unknown_host_default_background_transparent() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"hi");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Reset));
        assert_eq!(buffer[(0, 0)].style().bg, Some(Color::Reset));
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Reset));
        assert_eq!(buffer[(2, 0)].style().bg, Some(Color::Reset));
    }

    #[test]
    fn render_blanks_kitty_unicode_placeholders() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal
                .write("before\u{10eeee}\u{0305}\u{0305}after".as_bytes());
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "b");
        assert_eq!(buffer[(6, 0)].symbol(), " ");
        assert_eq!(buffer[(7, 0)].symbol(), "a");
        assert_eq!(pane.visible_text().lines().next(), Some("before after"));
        assert_eq!(pane.recent_text(5), "before after\n");
    }

    #[test]
    fn render_keeps_explicit_cell_foreground_when_host_is_unknown() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b[38;2;68;85;102mhi\x1b[0m");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        let expected_fg = Some(Color::Rgb(0x44, 0x55, 0x66));
        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(0, 0)].style().fg, expected_fg);
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Reset));
    }

    #[test]
    fn render_keeps_explicit_cell_background_when_host_is_unknown() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b[48;2;68;85;102mhi\x1b[0m");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        let expected_bg = Some(Color::Rgb(0x44, 0x55, 0x66));
        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(0, 0)].style().bg, expected_bg);
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].style().bg, Some(Color::Reset));
    }

    #[test]
    fn render_preserves_palette_colors_instead_of_flattening_to_rgb() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(
                b"\x1b[31mR\x1b[0m \x1b[38;5;171mI\x1b[0m \x1b[48;5;4mB\x1b[0m \x1b[38;2;1;2;3mT",
            );
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "R");
        assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Indexed(1)));
        assert_eq!(buffer[(2, 0)].symbol(), "I");
        assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Indexed(171)));
        assert_eq!(buffer[(4, 0)].symbol(), "B");
        assert_eq!(buffer[(4, 0)].style().bg, Some(Color::Indexed(4)));
        assert_eq!(buffer[(6, 0)].symbol(), "T");
        assert_eq!(buffer[(6, 0)].style().fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn render_preserves_palette_background_fill_cells() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b[48;5;4m\x1b[K");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        for x in 0..20 {
            assert_eq!(buffer[(x, 0)].symbol(), " ");
            assert_eq!(buffer[(x, 0)].style().bg, Some(Color::Indexed(4)));
        }
    }

    #[test]
    fn render_preserves_rgb_background_fill_cells() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b[48;2;17;34;51m\x1b[K");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        for x in 0..20 {
            assert_eq!(buffer[(x, 0)].symbol(), " ");
            assert_eq!(buffer[(x, 0)].style().bg, Some(Color::Rgb(17, 34, 51)));
        }
    }

    #[test]
    fn process_pty_bytes_does_not_advertise_unsupported_glyph_protocol() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b_25a1;s\x1b\\");

        assert!(result.terminal_responses.is_empty());
    }

    #[test]
    fn process_pty_bytes_returns_core_query_responses_without_queuing_input() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[6n");

        assert_eq!(result.terminal_responses.len(), 1);
        assert!(String::from_utf8_lossy(&result.terminal_responses[0]).contains('R'));
    }

    #[test]
    fn color_scheme_queries_and_live_updates_follow_terminal_mode() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        assert!(
            pane.apply_host_terminal_appearance(Some(
                crate::host_term::theme::HostAppearance::Dark
            ))
            .is_none()
        );
        let query = pane.process_pty_bytes(pane_id, 0, b"\x1b[?996n");
        assert_eq!(
            query.terminal_responses,
            vec![Bytes::from_static(b"\x1b[?997;1n")]
        );

        pane.process_pty_bytes(pane_id, 0, b"\x1b[?2031h");
        assert!(
            pane.apply_host_terminal_appearance(Some(
                crate::host_term::theme::HostAppearance::Dark
            ))
            .is_none()
        );
        assert_eq!(
            pane.apply_host_terminal_appearance(Some(
                crate::host_term::theme::HostAppearance::Light
            )),
            Some(Bytes::from_static(b"\x1b[?997;2n"))
        );

        assert!(pane.apply_host_terminal_appearance(None).is_none());
        let unknown_query = pane.process_pty_bytes(pane_id, 0, b"\x1b[?996n");
        assert!(unknown_query.terminal_responses.is_empty());
        assert!(
            pane.apply_host_terminal_appearance(Some(
                crate::host_term::theme::HostAppearance::Dark
            ))
            .is_none()
        );

        pane.process_pty_bytes(pane_id, 0, b"\x1bc");
        assert!(
            pane.apply_host_terminal_appearance(Some(
                crate::host_term::theme::HostAppearance::Light
            ))
            .is_none()
        );
    }

    #[test]
    fn process_pty_bytes_returns_xtgettcap_truecolor_query_responses_without_queuing_input() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(
            pane_id,
            0,
            b"\x1bP+q5463;524742;73657472676266;73657472676262\x1b\\",
        );

        assert_eq!(
            result.terminal_responses,
            vec![
                expected_xtgettcap_response("5463", None),
                expected_xtgettcap_response("524742", Some(b"8")),
                expected_xtgettcap_response("73657472676266", Some(b"\\E[38:2:%p1%d:%p2%d:%p3%dm")),
                expected_xtgettcap_response("73657472676262", Some(b"\\E[48:2:%p1%d:%p2%d:%p3%dm")),
            ]
        );
    }

    #[test]
    fn process_pty_bytes_returns_fragmented_c1_xtgettcap_once_in_order() {
        // Raw C1 bytes (0x90 here) are text/no-ops to the 7-bit vte parser
        // and never open a DCS: only the ESC-introduced form is a real
        // query. See the framing note atop `ghostty/scan.rs`.
        for (query, opens_dcs) in [
            (b"\x90+q5463;524742\x9c".as_slice(), false),
            (b"\x1bP+q5463;524742\x9c".as_slice(), true),
            (b"\x90+q5463;524742\x1b\\".as_slice(), false),
        ] {
            for fragmented in [false, true] {
                let terminal = crate::ghostty::Terminal::new(20, 5, 0);
                let pane = GhosttyPaneTerminal::new(terminal);
                let pane_id = PaneId::from_raw(1);
                pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
                    background: Some(crate::host_term::theme::RgbColor {
                        r: 0,
                        g: 0x2b,
                        b: 0x36,
                    }),
                    ..Default::default()
                });
                let mut replies = pane
                    .process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07")
                    .terminal_responses;
                for chunk in query.chunks(if fragmented { 1 } else { query.len() }) {
                    replies.extend(pane.process_pty_bytes(pane_id, 0, chunk).terminal_responses);
                }
                replies.extend(
                    pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x1b\\\x1bP+q5375\x1b\\")
                        .terminal_responses,
                );
                let expected = if opens_dcs {
                    vec![
                        Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                        expected_xtgettcap_response("5463", None),
                        expected_xtgettcap_response("524742", Some(b"8")),
                        Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                        expected_xtgettcap_response("5375", None),
                    ]
                } else {
                    vec![
                        Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                        Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
                        expected_xtgettcap_response("5375", None),
                    ]
                };
                assert_eq!(
                    replies, expected,
                    "query={query:?}, fragmented={fragmented}"
                );
            }
        }
    }

    #[test]
    fn process_pty_bytes_returns_split_xtgettcap_query_response() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q4");
        assert!(result.terminal_responses.is_empty());
        let result = pane.process_pty_bytes(pane_id, 0, b"d73");
        assert!(result.terminal_responses.is_empty());
        // The parser ends DCS on ESC, before the final ST backslash.
        // Splitting ST must not lose the reply or emit it again on completion.
        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b");

        assert_eq!(
            result.terminal_responses,
            vec![expected_xtgettcap_response(
                "4D73",
                Some(b"\\E]52;%p1%s;%p2%s\\007")
            )]
        );
        let result = pane.process_pty_bytes(pane_id, 0, b"\\");
        assert!(result.terminal_responses.is_empty());
    }

    #[test]
    fn process_pty_bytes_orders_device_attribute_reply_before_following_xtgettcap_reply() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b[c\x1bP+q5463\x1b\\");

        assert_eq!(result.terminal_responses.len(), 2);
        assert!(String::from_utf8_lossy(&result.terminal_responses[0]).contains('c'));
        assert_eq!(
            result.terminal_responses[1],
            expected_xtgettcap_response("5463", None)
        );
    }

    #[test]
    fn process_pty_bytes_orders_xtgettcap_reply_before_following_device_attribute_reply() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q5463\x1b\\\x1b[c");

        assert_eq!(result.terminal_responses.len(), 2);
        assert_eq!(
            result.terminal_responses[0],
            expected_xtgettcap_response("5463", None)
        );
        assert!(String::from_utf8_lossy(&result.terminal_responses[1]).contains('c'));
    }

    #[test]
    fn process_pty_bytes_orders_xtgettcap_reply_before_following_default_color_reply() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x00,
                g: 0x2b,
                b: 0x36,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q5463\x1b\\\x1b]11;?\x07");

        assert_eq!(
            result.terminal_responses,
            vec![
                expected_xtgettcap_response("5463", None),
                Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\"),
            ]
        );
    }

    #[test]
    fn host_theme_update_preserves_child_default_color_override() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;#112233\x07");
        assert!(result.terminal_responses.is_empty());

        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07");
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]11;rgb:1111/2222/3333\x07")]
        );
    }

    #[test]
    fn child_default_color_reset_restores_cached_host_color() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"\x1b]11;#112233\x07");
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            ..Default::default()
        });
        pane.process_pty_bytes(pane_id, 0, b"\x1b]111\x07");
        assert!(!pane.has_transient_default_color_override());

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07");
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]11;rgb:aaaa/bbbb/cccc\x1b\\")]
        );
    }

    #[test]
    fn process_pty_bytes_recovers_xtgettcap_after_osc_bel_terminator() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]0;title\x07\x1bP+q5463\x1b\\");

        assert_eq!(
            result.terminal_responses,
            vec![expected_xtgettcap_response("5463", None)]
        );
    }

    #[test]
    fn process_pty_bytes_orders_default_color_reset_reply_before_xtgettcap() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x00,
                g: 0x2b,
                b: 0x36,
            }),
            ..Default::default()
        });

        // OSC ends at the ESC of its string terminator, so the reply to the
        // query arrives with the chunk that carries that ESC.
        let result =
            pane.process_pty_bytes(pane_id, 0, b"\x1b]11;#112233\x07\x1b]111\x07\x1b]11;?\x1b");
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")]
        );
        let result = pane.process_pty_bytes(pane_id, 0, b"\\\x1bP+q436f\x1b\\");

        assert_eq!(
            result.terminal_responses,
            vec![expected_xtgettcap_response("436F", Some(b"256"))]
        );
    }

    #[test]
    fn process_pty_bytes_ignores_unknown_and_unsupported_xtgettcap_queries() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1bP+q6E6F7065;4D7\x1b\\");

        assert!(result.terminal_responses.is_empty());
    }

    #[test]
    fn process_pty_bytes_returns_underline_color_xtgettcap_query_responses() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result =
            pane.process_pty_bytes(pane_id, 0, b"\x1bP+q5375;536D756C78;536574756C63\x1b\\");

        assert_eq!(
            result.terminal_responses,
            vec![
                expected_xtgettcap_response("5375", None),
                expected_xtgettcap_response("536D756C78", Some(b"\\E[4:%p1%dm")),
                expected_xtgettcap_response(
                    "536574756C63",
                    Some(b"\\E[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m")
                ),
            ]
        );
    }

    #[test]
    fn render_preserves_underline_color() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b[4m\x1b[58:2::17:34:51mU");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let style = terminal.backend().buffer()[(0, 0)].style();
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(style.underline_color, Some(Color::Rgb(17, 34, 51)));
    }

    #[test]
    fn full_frame_preserves_curly_underline_style() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b[4:3mU");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let frame =
            crate::protocol::FrameData::from_ratatui_buffer(terminal.backend().buffer(), None);
        assert_eq!(frame.cells[0].symbol, "U");
        assert_eq!(
            frame.cells[0].style.underline,
            crate::ghostty::UnderlineStyle::Curly
        );
    }

    #[test]
    fn process_pty_bytes_orders_default_color_reply_before_following_device_attribute_reply() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x00,
                g: 0x2b,
                b: 0x36,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07\x1b[c");

        assert_eq!(result.terminal_responses.len(), 2);
        assert_eq!(
            result.terminal_responses[0],
            Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")
        );
        assert!(String::from_utf8_lossy(&result.terminal_responses[1]).contains('c'));
    }

    #[test]
    fn process_pty_bytes_returns_host_palette_color_without_queuing_input() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(
            crate::host_term::theme::TerminalTheme::default().with_palette_color(
                0,
                crate::host_term::theme::RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                },
            ),
        );

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?\x07");

        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]4;0;rgb:1111/2222/3333\x1b\\")]
        );
    }

    #[test]
    fn opentui_256_palette_query_burst_uses_host_snapshot() {
        use std::fmt::Write as _;

        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        let mut theme = crate::host_term::theme::TerminalTheme::default();
        let mut queries = String::new();
        for index in 0..=u8::MAX {
            theme = theme.with_palette_color(
                index,
                crate::host_term::theme::RgbColor {
                    r: index,
                    g: 0x22,
                    b: 0x33,
                },
            );
            let _ = write!(queries, "\x1b]4;{index};?\x07");
        }
        pane.apply_host_terminal_theme(theme);

        let result = pane.process_pty_bytes(pane_id, 0, queries.as_bytes());

        assert_eq!(result.terminal_responses.len(), 256);
        assert_eq!(
            result.terminal_responses[0],
            Bytes::from_static(b"\x1b]4;0;rgb:0000/2222/3333\x1b\\")
        );
        assert_eq!(
            result.terminal_responses[255],
            Bytes::from_static(b"\x1b]4;255;rgb:ffff/2222/3333\x1b\\")
        );
    }

    #[test]
    fn child_palette_override_survives_host_refresh_until_reset() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(
            crate::host_term::theme::TerminalTheme::default().with_palette_color(
                7,
                crate::host_term::theme::RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                },
            ),
        );
        pane.process_pty_bytes(pane_id, 0, b"\x1b]4;7;rgb:aa/bb/cc\x1b\\");

        pane.apply_host_terminal_theme(
            crate::host_term::theme::TerminalTheme::default().with_palette_color(
                7,
                crate::host_term::theme::RgbColor {
                    r: 0x44,
                    g: 0x55,
                    b: 0x66,
                },
            ),
        );
        let overridden = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;7;?\x1b\\");
        assert_eq!(
            overridden.terminal_responses,
            vec![Bytes::from_static(b"\x1b]4;7;rgb:aaaa/bbbb/cccc\x1b\\")]
        );

        pane.process_pty_bytes(pane_id, 0, b"\x1b]104;7\x1b\\");
        let reset = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;7;?\x1b\\");
        assert_eq!(
            reset.terminal_responses,
            vec![Bytes::from_static(b"\x1b]4;7;rgb:4444/5555/6666\x1b\\")]
        );
    }

    #[test]
    fn process_pty_bytes_returns_split_palette_color_query_response() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        let color = current_palette_color(&pane, 255);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;25");
        assert!(result.terminal_responses.is_empty());
        // The OSC is complete at the ESC of its terminator.
        let result = pane.process_pty_bytes(pane_id, 0, b"5;?\x1b");
        assert_eq!(
            result.terminal_responses,
            vec![expected_osc_rgb_response("4;255", color)]
        );
        let result = pane.process_pty_bytes(pane_id, 0, b"\\");

        assert!(result.terminal_responses.is_empty());
    }

    #[test]
    fn process_pty_bytes_ignores_malformed_and_preserves_multi_palette_queries() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(
            pane_id,
            0,
            b"\x1b]4;;?\x07\x1b]4;-1;?\x07\x1b]4;256;?\x07\x1b]4;0;?;1;?\x07\x1b]4;0;rgb:1111/2222/3333\x07",
        );

        // A multi-entry query is answered one entry per reply.
        assert_eq!(result.terminal_responses.len(), 2);
        assert!(result.terminal_responses[0].starts_with(b"\x1b]4;0;rgb:"));
        assert!(result.terminal_responses[1].starts_with(b"\x1b]4;1;rgb:"));
    }

    #[test]
    fn process_pty_bytes_orders_palette_reply_before_following_terminal_replies() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        let color = current_palette_color(&pane, 0);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x00,
                g: 0x2b,
                b: 0x36,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?\x07\x1b]11;?\x07\x1b[c");

        assert_eq!(result.terminal_responses.len(), 3);
        assert_eq!(
            result.terminal_responses[0],
            expected_osc_rgb_response("4;0", color)
        );
        assert_eq!(
            result.terminal_responses[1],
            Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")
        );
        assert!(String::from_utf8_lossy(&result.terminal_responses[2]).contains('c'));
    }

    #[test]
    fn process_pty_bytes_returns_default_color_query_responses_without_queuing_input() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x00,
                g: 0x2b,
                b: 0x36,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;?\x07");

        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\")]
        );
    }

    #[test]
    fn process_pty_bytes_preserves_untracked_multi_color_query_responses() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0x65,
                g: 0x7b,
                b: 0x83,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0xfd,
                g: 0xf6,
                b: 0xe3,
            }),
            ..Default::default()
        });

        let palette = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?;1;?\x1b\\");
        let palette_response = palette.terminal_responses.concat();
        assert!(palette_response.starts_with(b"\x1b]4;0;rgb:"));
        assert_eq!(
            palette_response
                .windows(4)
                .filter(|window| *window == b"rgb:")
                .count(),
            2
        );

        let defaults = pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?;?;?\x1b\\");
        let default_response = defaults.terminal_responses.concat();
        assert!(
            default_response.starts_with(b"\x1b]10;rgb:"),
            "unexpected default-color report: {:?}",
            String::from_utf8_lossy(&default_response)
        );
        assert_eq!(
            default_response
                .windows(4)
                .filter(|window| *window == b"rgb:")
                .count(),
            3
        );
        let core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
        assert!(!has_default_color_override(&core.terminal));
        drop(core);
    }

    #[test]
    fn process_pty_bytes_preserves_earlier_aggregate_palette_reply() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]4;0;?;1;?\x1b\\\x1b]4;0;?\x1b\\");

        assert_eq!(result.terminal_responses.len(), 3);
        assert!(result.terminal_responses[0].starts_with(b"\x1b]4;0;rgb:"));
        assert!(result.terminal_responses[1].starts_with(b"\x1b]4;1;rgb:"));
        assert!(result.terminal_responses[2].starts_with(b"\x1b]4;0;rgb:"));
    }

    #[test]
    fn process_pty_bytes_preserves_core_reply_for_child_color_override() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"\x1b]10;rgb:11/22/33\x07");
        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?\x1b\\");

        assert_eq!(result.terminal_responses.len(), 1);
        assert!(result.terminal_responses[0].starts_with(b"\x1b]10;rgb:1111/2222/3333"));
    }

    #[test]
    fn process_pty_bytes_tracks_later_multi_value_color_set() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);

        pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?;rgb:44/55/66\x1b\\");

        let core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
        assert_eq!(
            core.terminal
                .default_color_override(crate::ghostty::DefaultColor::Foreground),
            None
        );
        assert_eq!(
            core.terminal
                .default_color_override(crate::ghostty::DefaultColor::Background),
            Some(rgb(0x44, 0x55, 0x66))
        );
    }

    #[test]
    fn process_pty_bytes_returns_cursor_color_query_response_from_foreground_fallback() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0x65,
                g: 0x7b,
                b: 0x83,
            }),
            background: None,
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12;?\x07");

        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]12;rgb:6565/7b7b/8383\x1b\\")]
        );
    }

    #[test]
    fn process_pty_bytes_returns_cursor_color_query_response_from_child_foreground() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0x65,
                g: 0x7b,
                b: 0x83,
            }),
            background: None,
            ..Default::default()
        });

        pane.process_pty_bytes(pane_id, 0, b"\x1b]10;rgb:11/22/33\x07");
        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12;?\x07");

        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]12;rgb:1111/2222/3333\x1b\\")]
        );
    }

    #[test]
    fn process_pty_bytes_returns_explicit_cursor_color_query_response() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0x65,
                g: 0x7b,
                b: 0x83,
            }),
            background: None,
            ..Default::default()
        });

        pane.process_pty_bytes(pane_id, 0, b"\x1b]12;rgb:11/22/33\x07");
        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12;?\x07");

        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]12;rgb:1111/2222/3333\x1b\\")]
        );
    }

    #[test]
    fn process_pty_bytes_returns_default_color_query_responses_in_order() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0x65,
                g: 0x7b,
                b: 0x83,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0xfd,
                g: 0xf6,
                b: 0xe3,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07");

        assert_eq!(
            result.terminal_responses,
            vec![
                Bytes::from_static(b"\x1b]10;rgb:6565/7b7b/8383\x1b\\"),
                Bytes::from_static(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\"),
                Bytes::from_static(b"\x1b]12;rgb:6565/7b7b/8383\x1b\\"),
            ]
        );
    }

    #[test]
    fn process_pty_bytes_returns_split_default_color_query_response() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0xfd,
                g: 0xf6,
                b: 0xe3,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11");
        assert!(result.terminal_responses.is_empty());
        let result = pane.process_pty_bytes(pane_id, 0, b";?\x1b");
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\")]
        );
        let result = pane.process_pty_bytes(pane_id, 0, b"\\");

        assert!(result.terminal_responses.is_empty());
    }

    #[test]
    fn process_pty_bytes_returns_split_cursor_color_query_response() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xfd,
                g: 0xf6,
                b: 0xe3,
            }),
            background: None,
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]12");
        assert!(result.terminal_responses.is_empty());
        let result = pane.process_pty_bytes(pane_id, 0, b";?\x1b");
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]12;rgb:fdfd/f6f6/e3e3\x1b\\")]
        );
        let result = pane.process_pty_bytes(pane_id, 0, b"\\");

        assert!(result.terminal_responses.is_empty());
    }

    #[test]
    fn process_pty_bytes_tracks_default_color_set_and_reset_before_replying() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.apply_host_terminal_theme(crate::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(crate::host_term::theme::RgbColor {
                r: 0xfd,
                g: 0xf6,
                b: 0xe3,
            }),
            ..Default::default()
        });

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]11;rgb:11/22/33\x07\x1b]11;?\x07");
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]11;rgb:1111/2222/3333\x07")]
        );

        let result = pane.process_pty_bytes(pane_id, 0, b"\x1b]111\x07\x1b]11;?\x07");
        assert_eq!(
            result.terminal_responses,
            vec![Bytes::from_static(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x1b\\")]
        );
    }

    #[test]
    fn render_leaves_host_default_background_transparent() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let host_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };
        pane.apply_host_terminal_theme(host_theme);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"hi");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(0, 0)].style().fg, Some(Color::Reset));
        assert_eq!(buffer[(0, 0)].style().bg, Some(Color::Reset));
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Reset));
        assert_eq!(buffer[(2, 0)].style().bg, Some(Color::Reset));
    }

    #[test]
    fn render_keeps_explicit_default_foreground_when_it_differs_from_host() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let host_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };
        pane.apply_host_terminal_theme(host_theme);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b]10;rgb:44/55/66\x1b\\hi");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        let expected_fg = Some(Color::Rgb(0x44, 0x55, 0x66));
        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(0, 0)].style().fg, expected_fg);
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].style().fg, expected_fg);
    }

    #[test]
    fn render_keeps_explicit_default_background_when_it_differs_from_host() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let host_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };
        pane.apply_host_terminal_theme(host_theme);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            core.terminal.write(b"\x1b]11;rgb:44/55/66\x1b\\hi");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        let expected_bg = Some(Color::Rgb(0x44, 0x55, 0x66));
        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(0, 0)].style().bg, expected_bg);
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].style().bg, expected_bg);
    }

    #[test]
    fn render_inverse_text_swaps_fg_and_resolved_bg_when_bg_is_transparent() {
        let terminal = crate::ghostty::Terminal::new(20, 5, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let host_theme = crate::host_term::theme::TerminalTheme {
            foreground: Some(crate::host_term::theme::RgbColor {
                r: 0xaa,
                g: 0xbb,
                b: 0xcc,
            }),
            background: Some(crate::host_term::theme::RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
            ..Default::default()
        };
        pane.apply_host_terminal_theme(host_theme);
        {
            let mut core =
                crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
            // SGR 7 enables inverse/reverse video
            core.terminal.write(b"\x1b[7mhi\x1b[27m");
        }

        let backend = ratatui::backend::TestBackend::new(20, 5);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 20, 5), false))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        let cell = &buffer[(0, 0)];
        assert_eq!(cell.symbol(), "h");
        // After inverse: fg should be the resolved bg, bg should be the original fg.
        // fg must NOT be Color::Reset (which would be the same hue as bg).
        assert_eq!(cell.style().fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert_eq!(cell.style().bg, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
    }

    #[test]
    fn trim_trailing_blank_rows_drops_empty_viewport_tail() {
        let mut rows = vec!["hello".to_string(), String::new(), "   ".to_string()];
        trim_trailing_blank_rows(&mut rows);
        assert_eq!(rows, vec!["hello".to_string()]);
    }

    /// Once history is full every line of output evicts one: screen rows
    /// drift onto other lines, absolute rows stay on theirs.
    #[test]
    fn absolute_rows_survive_eviction_where_screen_rows_drift() {
        // One byte of budget buys the minimum history.
        let mut terminal = crate::ghostty::Terminal::new(10, 3, 1);
        write_numbered_lines(&mut terminal, 1_100);
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
        let pane_id = PaneId::from_raw(1);
        let position = pane.scroll_position().expect("test precondition");
        assert!(
            position.metrics.history_origin > AbsRow(0),
            "history must be full"
        );
        assert_eq!(
            position.viewport_top_row(),
            position.metrics.history_origin.saturating_add(
                u64::try_from(position.metrics.max_offset_from_bottom).expect("fits"),
            )
        );

        // Line i was written on absolute row i.
        let found = pane.search_text_window_absolute(
            "001050",
            true,
            TerminalSearchDirection::Forward,
            TerminalTextPoint {
                row: AbsRow(0),
                col: 0,
            },
            None,
            8,
        );
        assert_eq!(found.total, 1);
        let line = found.matches[0].start.row;
        assert_eq!(line, AbsRow(1_050));
        let selection = crate::selection::Selection::range(
            PaneId::from_raw(1),
            Point::new(line, 0),
            Point::new(line, 5),
        );
        assert_eq!(
            pane.extract_selection(&selection).as_deref(),
            Some("001050")
        );

        for i in 1_100..1_150 {
            pane.process_pty_bytes(pane_id, 0, format!("{i:06}\r\n").as_bytes());
        }
        assert_eq!(
            pane.scroll_position()
                .expect("test precondition")
                .metrics
                .history_origin,
            position.metrics.history_origin.saturating_add(50)
        );
        assert_eq!(
            pane.extract_selection(&selection).as_deref(),
            Some("001050")
        );
        assert_eq!(
            pane.word_motion_target_absolute(line, 0, TerminalWordMotion::NextEnd),
            Some(TerminalTextPoint { row: line, col: 5 })
        );
        // The screen-row entry points agree with the absolute ones at the
        // moment they are called.
        let origin = pane
            .scroll_position()
            .expect("test precondition")
            .metrics
            .history_origin;
        let now = line.screen_row(origin).expect("line remains retained");
        assert_eq!(
            pane.word_motion_target(now, 0, TerminalWordMotion::NextEnd),
            Some(TerminalTextPoint { row: now, col: 5 })
        );

        // An evicted row is refused rather than read.
        let evicted = origin.saturating_sub(1);
        let gone = crate::selection::Selection::range(
            PaneId::from_raw(1),
            Point::new(evicted, 0),
            Point::new(evicted, 5),
        );
        assert_eq!(pane.extract_selection(&gone), None);
        assert_eq!(
            pane.word_motion_target_absolute(evicted, 0, TerminalWordMotion::NextStart),
            None
        );
        assert_eq!(pane.paragraph_motion_target_absolute(evicted, 1), None);
    }

    #[test]
    fn paragraph_motion_finds_blank_rows_by_absolute_row() {
        let mut terminal = crate::ghostty::Terminal::new(10, 3, 1);
        write_numbered_lines(&mut terminal, 1_100);
        terminal.write(b"\r\npara\r\ngraph");
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
        let origin = pane
            .scroll_position()
            .expect("test precondition")
            .metrics
            .history_origin;
        // Rows: ..., 001099 on row 1099, blank on 1100, "para" on 1101.
        assert_eq!(
            pane.paragraph_motion_target_absolute(AbsRow(1_101), -1),
            Some(TerminalTextPoint {
                row: AbsRow(1_100),
                col: 0
            })
        );
        let para = AbsRow(1_101)
            .screen_row(origin)
            .expect("row remains retained");
        assert_eq!(
            pane.paragraph_motion_target(para, -1),
            Some(TerminalTextPoint {
                row: ScreenRow(para.0 - 1),
                col: 0
            })
        );
    }

    /// The streaming search reads the grid in chunks under short lock holds;
    /// it must find exactly what a search of the whole buffer at once finds,
    /// including a match whose soft-wrapped rows straddle a chunk boundary.
    #[test]
    fn chunked_search_matches_a_whole_buffer_search() {
        let mut terminal = crate::ghostty::Terminal::new(10, 3, 1_000_000);
        write_numbered_lines(&mut terminal, 2_046);
        terminal.write(b"abcdefghijklmnopqrstuvwxyz0123\r\n");
        for i in 3_000..3_100 {
            terminal.write(format!("{i:06}\r\n").as_bytes());
        }
        assert_eq!(
            terminal.history_origin(),
            AbsRow(0),
            "history must not be full"
        );
        let whole = RetainedTextBuffer::new(terminal.cols(), terminal.screen_text_rows());
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));

        let at = |row: u64| TerminalTextPoint {
            row: AbsRow(row),
            col: 0,
        };
        for (query, direction, cursor) in [
            ("00", TerminalSearchDirection::Forward, at(1_000)),
            ("00", TerminalSearchDirection::Backward, at(1_000)),
            ("00", TerminalSearchDirection::Forward, at(9_999)),
            ("00", TerminalSearchDirection::Backward, at(0)),
            (
                "abcdefghijklmnopqrstuvwxyz",
                TerminalSearchDirection::Forward,
                at(0),
            ),
            ("nothing", TerminalSearchDirection::Forward, at(0)),
        ] {
            let expected = whole.search_window(
                query,
                true,
                crate::ghostty::ActiveScreen::Primary,
                direction,
                cursor,
                None,
                16,
            );
            let actual = pane.search_text_window_absolute(query, true, direction, cursor, None, 16);
            assert_eq!(actual, expected, "{query} {direction:?} from {cursor:?}");
        }
        let word = pane.search_text_window_absolute(
            "abcdefghijklmnopqrstuvwxyz",
            true,
            TerminalSearchDirection::Forward,
            at(0),
            None,
            1,
        );
        assert_eq!(
            (word.matches[0].start, word.matches[0].end),
            (
                TerminalTextPoint {
                    row: AbsRow(2_046),
                    col: 0
                },
                TerminalTextPoint {
                    row: AbsRow(2_048),
                    col: 5
                }
            )
        );
    }

    /// The window kept while matches stream past agrees with slicing the
    /// complete match list, for every target position and window size.
    #[test]
    fn match_window_agrees_with_the_complete_match_list() {
        let all: Vec<TerminalTextMatch<AbsRow>> = (0..40u64)
            .map(|row| TerminalTextMatch {
                start: TerminalTextPoint {
                    row: AbsRow(row),
                    col: 2,
                },
                end: TerminalTextPoint {
                    row: AbsRow(row),
                    col: 4,
                },
                source_fingerprint: row,
                scan_cols: 10,
                scan_screen: crate::ghostty::ActiveScreen::Primary,
            })
            .collect();
        let complete = |direction: TerminalSearchDirection,
                        origin: TerminalTextPoint<AbsRow>,
                        limit: usize| {
            let mut target = None;
            for (index, text_match) in all.iter().enumerate() {
                match direction {
                    TerminalSearchDirection::Forward
                        if target.is_none() && text_match.start > origin =>
                    {
                        target = Some(index);
                    }
                    TerminalSearchDirection::Backward if text_match.end < origin => {
                        target = Some(index);
                    }
                    _ => {}
                }
            }
            let total = all.len();
            let target = target.unwrap_or(match direction {
                TerminalSearchDirection::Forward => 0,
                TerminalSearchDirection::Backward => total - 1,
            });
            let retained = limit.min(total);
            let start = target.saturating_sub(retained / 2).min(total - retained);
            TerminalSearchWindow {
                matches: all[start..start + retained].to_vec(),
                current: Some(target - start),
                current_global: Some(target),
                total,
            }
        };
        for direction in [
            TerminalSearchDirection::Forward,
            TerminalSearchDirection::Backward,
        ] {
            for origin_row in [0u64, 1, 7, 20, 38, 39, 45] {
                for origin_col in [0u16, 3, 9] {
                    for limit in [1usize, 2, 3, 7, 16, 39, 40, 100] {
                        let origin = TerminalTextPoint {
                            row: AbsRow(origin_row),
                            col: origin_col,
                        };
                        let mut window = MatchWindow {
                            direction,
                            origin,
                            limit,
                            total: 0,
                            target: None,
                            first: Vec::new(),
                            recent: VecDeque::new(),
                            boundary: None,
                            after: Vec::new(),
                        };
                        for text_match in &all {
                            window.push(*text_match);
                        }
                        assert_eq!(
                            window.finish(),
                            complete(direction, origin, limit),
                            "{direction:?} from {origin:?}, limit {limit}"
                        );
                    }
                }
            }
        }
    }

    /// A full render draws every row and must not consume the dirty rows
    /// the next patch still has to send.
    #[test]
    fn full_render_leaves_dirty_rows_for_the_next_patch() {
        let terminal = crate::ghostty::Terminal::new(8, 4, 100);
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
        let pane_id = PaneId::from_raw(1);
        pane.collect_dirty_patch(8, 4);
        pane.process_pty_bytes(pane_id, 0, b"\x1b[2;1HX");

        let backend = ratatui::backend::TestBackend::new(8, 4);
        let mut host = ratatui::Terminal::new(backend).expect("test precondition");
        host.draw(|frame| pane.render(frame, Rect::new(0, 0, 8, 4), false))
            .expect("test precondition");

        let TerminalDirtyPatchOutcome::Patch(patch) = pane.collect_dirty_patch(8, 4) else {
            panic!("the row written before the render must still be sent");
        };
        assert!(patch.rows.iter().any(|(y, _)| *y == 1));
    }

    /// Rows below a patch's area stay dirty, and so does the overall state:
    /// a taller patch later still sends them.
    #[test]
    fn rows_below_a_patch_area_are_sent_by_a_later_taller_patch() {
        let terminal = crate::ghostty::Terminal::new(8, 6, 100);
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
        let pane_id = PaneId::from_raw(1);
        pane.collect_dirty_patch(8, 6);
        pane.process_pty_bytes(pane_id, 0, b"\x1b[2;3HX\x1b[5;4HY");

        let TerminalDirtyPatchOutcome::Patch(short) = pane.collect_dirty_patch(8, 3) else {
            panic!("expected a patch");
        };
        assert_eq!(short.rows.iter().map(|(y, _)| *y).collect::<Vec<_>>(), [1]);
        assert!(
            !matches!(
                pane.collect_dirty_patch(8, 3),
                TerminalDirtyPatchOutcome::Clean
            ),
            "a row is still waiting below the area"
        );
        let TerminalDirtyPatchOutcome::Patch(tall) = pane.collect_dirty_patch(8, 6) else {
            panic!("expected a patch");
        };
        assert_eq!(tall.rows.iter().map(|(y, _)| *y).collect::<Vec<_>>(), [4]);
        assert!(matches!(
            pane.collect_dirty_patch(8, 6),
            TerminalDirtyPatchOutcome::Clean
        ));
    }

    #[test]
    fn default_color_changes_ask_for_an_owner_only_while_an_override_stands() {
        let terminal = crate::ghostty::Terminal::new(20, 3, 0);
        let pane = GhosttyPaneTerminal::new(terminal);
        let mut core = crate::ghostty::lock_terminal_core(&pane.core).expect("test precondition");
        core.terminal.write(b"\x1b]11;rgb:10/20/30\x07");
        assert!(note_default_color_change(&mut core));
        // Nothing new since.
        assert!(!note_default_color_change(&mut core));

        core.transient_default_color_owner_pgid = Some(42);
        core.terminal.write(b"\x1b]111\x07");
        assert!(!note_default_color_change(&mut core));
        assert_eq!(core.transient_default_color_owner_pgid, None);
    }

    #[test]
    fn primary_history_is_unavailable_on_the_alternate_screen() {
        let terminal = crate::ghostty::Terminal::new(20, 3, 100_000);
        let pane = PaneTerminal::new(GhosttyPaneTerminal::new(terminal));
        let pane_id = PaneId::from_raw(1);
        pane.process_pty_bytes(pane_id, 0, b"history one\r\nhistory two\r\nprompt");
        assert!(
            pane.primary_history_ansi()
                .is_some_and(|ansi| ansi.contains("history one"))
        );

        pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049h\x1b[2J\x1b[Hfull-screen frame");
        assert_eq!(pane.primary_history_ansi(), None);

        pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049l");
        assert!(
            pane.primary_history_ansi()
                .is_some_and(|ansi| ansi.contains("history one") && !ansi.contains("full-screen"))
        );
    }

    #[test]
    fn screen_text_snapshot_copies_rows_only_on_the_alternate_screen() {
        let terminal = crate::ghostty::Terminal::new(20, 3, 100_000);
        let pane = GhosttyPaneTerminal::new(terminal);
        let pane_id = PaneId::from_raw(1);
        pane.process_pty_bytes(pane_id, 0, b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        let (screen, cols, rows) = pane.screen_text_snapshot().expect("snapshot");
        assert_eq!(screen, crate::ghostty::ActiveScreen::Primary);
        assert_eq!(cols, 20);
        assert!(rows.is_empty());

        pane.process_pty_bytes(pane_id, 0, b"\x1b[?1049h\x1b[2J\x1b[Hframe");
        let (screen, _, rows) = pane.screen_text_snapshot().expect("snapshot");
        assert_eq!(screen, crate::ghostty::ActiveScreen::Alternate);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn a_core_poisoned_off_the_reader_is_reported_to_the_reader() {
        let terminal = crate::ghostty::Terminal::new(20, 3, 0);
        let pane = std::sync::Arc::new(PaneTerminal::new(GhosttyPaneTerminal::new(terminal)));
        let pane_id = PaneId::from_raw(1);
        assert!(!pane.process_pty_bytes(pane_id, 0, b"before").core_poisoned);
        assert!(!pane.core_poisoned());

        // A render or API read panicking while it holds the core lock.
        let poisoner = std::sync::Arc::clone(&pane);
        let joined = std::thread::spawn(move || {
            let _core = crate::ghostty::lock_terminal_core(&poisoner.ghostty.core);
            panic!("panic while holding the core lock");
        })
        .join();
        assert!(joined.is_err(), "test precondition");

        // Visible without any output, for the actor's idle check.
        assert!(pane.core_poisoned());
        assert!(pane.process_pty_bytes(pane_id, 0, b"after").core_poisoned);
    }
}
