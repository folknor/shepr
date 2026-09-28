use crate::terminal::TerminalReadSnapshot;
pub use shepr_protocol::ScrollMetrics;
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

use shepr_core::layout::PaneId;
use shepr_protocol::CellData;
use shepr_vt::{AbsRow, Point, ScreenRow, ViewportRow};

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
/// Scroll metrics together with the row origin read under one terminal lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollPosition {
    pub metrics: ScrollMetrics,
}

impl ScrollPosition {
    #[cfg(test)]
    pub(crate) fn viewport_top_row(self) -> shepr_vt::AbsRow {
        self.metrics.viewport_top_row()
    }
}

/// A cell position in terminal text. `R` distinguishes the retained-buffer
/// index from a stable absolute row identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TerminalTextPoint<R = ScreenRow> {
    pub row: R,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTextMatch<R = ScreenRow> {
    pub start: TerminalTextPoint<R>,
    pub end: TerminalTextPoint<R>,
    pub source_fingerprint: u64,
    pub scan_cols: u16,
    pub scan_screen: shepr_vt::ActiveScreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSearchDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSearchWindow<R = ScreenRow> {
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
pub enum TerminalWordMotion {
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
    pub shape: shepr_protocol::CursorShapeParam,
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
pub struct TerminalDirtyPatch {
    pub rows: Vec<(u16, Vec<CellData>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalDirtyPatchOutcome {
    Clean,
    Patch(TerminalDirtyPatch),
    Fallback,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputState {
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub mouse_protocol_mode: shepr_termio::input::MouseProtocolMode,
    pub mouse_protocol_encoding: shepr_termio::input::MouseProtocolEncoding,
    pub mouse_alternate_scroll: bool,
    #[serde(default)]
    pub modify_other_keys: bool,
    #[serde(default)]
    pub color_scheme_reporting: bool,
}

#[cfg(test)]
impl InputState {
    pub(crate) fn mouse_reporting_enabled(self) -> bool {
        self.mouse_protocol_mode != shepr_termio::input::MouseProtocolMode::None
    }

    pub(crate) fn plain_page_keys_use_host_scrollback(self) -> bool {
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
    /// Runs inside the next dirty-patch collection; see
    /// `PaneRuntime::on_next_dirty_collection`.
    pub dirty_collection_hook: Option<Box<dyn FnOnce() + Send>>,
    pub terminal: shepr_vt::Terminal,
    synchronized_output_epoch: u64,
    pub render_state: shepr_vt::RenderState,
    pub initial_default_foreground: Option<shepr_vt::RgbColor>,
    pub initial_default_background: Option<shepr_vt::RgbColor>,
    pub host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
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

    pub(crate) fn on_next_dirty_collection(&self, hook: Box<dyn FnOnce() + Send>) {
        if let Ok(mut core) = shepr_vt::lock_terminal_core(&self.ghostty.core) {
            core.dirty_collection_hook = Some(hook);
        }
    }

    /// Whether a panic while holding the core lock has broken the core. A
    /// single atomic load, taking no lock: the PTY actor asks on every loop.
    pub(crate) fn core_poisoned(&self) -> bool {
        shepr_vt::terminal_core_is_poisoned(&self.ghostty.core)
    }

    pub(crate) fn process_pty_bytes(
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

    /// Flush an expired synchronized update. The runtime calls this before
    /// parsing later child output and from its timeout task; render and read
    /// paths do not flush implicitly.
    pub(crate) fn tick(&self, now: Instant) -> ProcessBytesResult {
        self.ghostty.tick(now)
    }

    pub(crate) fn resize(&self, geometry: shepr_core::geometry::PaneGeometry) -> Vec<Bytes> {
        self.ghostty.resize(geometry)
    }

    pub(crate) fn scroll_up(&self, lines: usize) {
        self.ghostty.scroll_up(lines);
    }

    pub(crate) fn scroll_down(&self, lines: usize) {
        self.ghostty.scroll_down(lines);
    }

    pub(crate) fn scroll_reset(&self) {
        self.ghostty.scroll_reset();
    }

    pub(crate) fn clear_screen(&self) -> Result<(), PaneClearError> {
        self.ghostty.clear_screen()
    }

    pub(crate) fn set_scroll_offset_from_bottom(&self, lines: usize) {
        self.ghostty.set_scroll_offset_from_bottom(lines);
    }

    pub(crate) fn scroll_metrics(&self) -> Option<ScrollMetrics> {
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
        let core = shepr_vt::lock_terminal_core(&self.ghostty.core).ok()?;
        let origin = core.terminal.history_origin();
        let target = word_motion_in(
            &core.terminal,
            absolute_point(TerminalTextPoint { row, col }, origin),
            motion,
        )?;
        Some(screen_point(target, origin))
    }

    pub(crate) fn dimensions(&self) -> Option<(u16, u16)> {
        let core = shepr_vt::lock_terminal_core(&self.ghostty.core).ok()?;
        Some((core.terminal.cols(), core.terminal.rows()))
    }

    /// Paragraph motion with screen rows; see
    /// [`PaneTerminal::paragraph_motion_target_absolute`].
    pub(crate) fn paragraph_motion_target(
        &self,
        row: ScreenRow,
        direction: i8,
    ) -> Option<TerminalTextPoint> {
        let core = shepr_vt::lock_terminal_core(&self.ghostty.core).ok()?;
        let origin = core.terminal.history_origin();
        let absolute = row.absolute(origin);
        let target = paragraph_motion_in(&core.terminal, absolute, direction)?;
        Some(screen_point(target, origin))
    }

    #[cfg(test)]
    pub(crate) fn input_state(&self) -> Option<InputState> {
        self.ghostty.input_state()
    }

    pub(crate) fn bracketed_paste_enabled(&self) -> bool {
        self.ghostty.bracketed_paste_enabled()
    }

    pub(crate) fn focus_reporting_enabled(&self) -> bool {
        self.ghostty.focus_reporting_enabled()
    }

    pub(crate) fn mouse_reporting_enabled(&self) -> bool {
        self.ghostty.mouse_reporting_enabled()
    }

    pub(crate) fn modify_other_keys_level(&self) -> u8 {
        self.ghostty.modify_other_keys_level()
    }

    pub(crate) fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.ghostty.sgr_pixel_mouse_enabled()
    }

    pub(crate) fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.ghostty.plain_page_keys_use_host_scrollback()
    }

    pub(crate) fn alternate_screen_active(&self) -> bool {
        self.ghostty.alternate_screen_active()
    }

    pub(crate) fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.ghostty.wheel_routing()
    }

    pub(crate) fn screen_text_snapshot(
        &self,
    ) -> Option<(shepr_vt::ActiveScreen, u16, Vec<shepr_vt::ScreenTextRow>)> {
        self.ghostty.screen_text_snapshot()
    }

    pub(crate) fn cursor_state(&self) -> Option<TerminalCursorState> {
        self.ghostty.cursor_state()
    }

    pub(crate) fn synchronized_output_active(&self) -> bool {
        self.ghostty.synchronized_output_active()
    }

    pub(crate) fn synchronized_output_state(&self) -> (bool, u64) {
        self.ghostty.synchronized_output_state()
    }

    pub(crate) fn visible_text(&self) -> String {
        self.ghostty.visible_text()
    }

    pub(crate) fn visible_ansi(&self) -> String {
        self.ghostty.visible_ansi()
    }

    pub(crate) fn detection_text(&self) -> String {
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
    pub(crate) fn extract_selection<P>(
        &self,
        selection: &shepr_vt::selection::Selection<P>,
    ) -> Option<String> {
        self.ghostty.extract_selection(selection)
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, show_cursor: bool) {
        self.ghostty.render(frame, area, show_cursor);
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> TerminalDirtyPatchOutcome {
        self.ghostty.collect_dirty_patch(area_width, area_height)
    }

    pub(crate) fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        self.ghostty.visible_hyperlinks(area)
    }

    pub(crate) fn apply_host_terminal_theme(
        &self,
        theme: shepr_termio::host_term::theme::TerminalTheme,
    ) {
        self.ghostty.apply_host_terminal_theme(theme);
    }

    pub(crate) fn apply_host_terminal_appearance(
        &self,
        appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
    ) -> Option<Bytes> {
        self.ghostty.apply_host_terminal_appearance(appearance)
    }

    pub(crate) fn has_transient_default_color_override(&self) -> bool {
        self.ghostty.has_transient_default_color_override()
    }

    pub(crate) fn maybe_restore_host_terminal_theme(
        &self,
        pane_id: PaneId,
        shell_pid: u32,
    ) -> bool {
        self.ghostty
            .maybe_restore_host_terminal_theme(pane_id, shell_pid)
    }

    pub(crate) fn terminal_title(&self) -> Option<String> {
        self.ghostty.terminal_title()
    }

    pub(crate) fn agent_osc_title(&self) -> String {
        self.ghostty.agent_osc_title()
    }

    pub(crate) fn agent_osc_progress(&self) -> String {
        self.ghostty.agent_osc_progress()
    }

    /// Clears retained OSC title/progress evidence on foreground agent change.
    pub(crate) fn clear_agent_osc_state(&self) {
        self.ghostty.clear_agent_osc_state();
    }

    pub(crate) fn keyboard_protocol(
        &self,
        fallback: shepr_termio::input::KeyboardProtocol,
    ) -> shepr_termio::input::KeyboardProtocol {
        self.ghostty.keyboard_protocol().unwrap_or(fallback)
    }

    pub(crate) fn encode_terminal_key(
        &self,
        key: shepr_termio::input::TerminalKey,
        protocol: shepr_termio::input::KeyboardProtocol,
    ) -> Vec<u8> {
        self.ghostty.encode_terminal_key(key, protocol)
    }

    pub(crate) fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.ghostty.encode_mouse_button(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.ghostty.encode_mouse_motion(kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.ghostty.encode_mouse_wheel(kind, position, modifiers)
    }
}

/// Direct readers addressed by stable absolute row IDs, plus primary history
/// reads for session persistence.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the copy-search and copy-motion endpoints use screen rows; these helpers \
                  keep the absolute-row operations directly testable without scroll metrics"
    )
)]
impl PaneTerminal {
    /// The viewport position and the absolute row id of screen row 0, read
    /// together; see [`ScrollPosition`].
    pub(crate) fn scroll_position(&self) -> Option<ScrollPosition> {
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
        let core = shepr_vt::lock_terminal_core(&self.ghostty.core).ok()?;
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
        let core = shepr_vt::lock_terminal_core(&self.ghostty.core).ok()?;
        paragraph_motion_in(&core.terminal, row, direction)
    }

    /// The whole primary-screen history as unwrapped ANSI, for session
    /// persistence. `None` while the alternate screen is active: alacritty
    /// gives no access to the inactive primary grid, and the active grid then
    /// holds the full-screen program's frame, which must not overwrite the
    /// history saved earlier.
    pub(crate) fn primary_history_ansi(&self) -> Option<String> {
        self.ghostty.primary_history_ansi()
    }
}

mod backend;
mod helpers;
#[cfg(test)]
mod tests;
mod text;

use helpers::*;
use text::*;
