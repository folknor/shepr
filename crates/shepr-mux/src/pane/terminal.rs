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
/// Only tests read it; production paths take [`ScrollMetrics`] directly.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollPosition {
    pub metrics: ScrollMetrics,
}

#[cfg(test)]
impl ScrollPosition {
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

pub(crate) struct PaneTerminal {
    /// Poisoned for good once anything panics while holding it. The readers
    /// below then answer empty or default values rather than error: the PTY
    /// actor checks `is_poisoned` on every loop (idle polls included, so at
    /// least once a second) and ends the pane, which is reported dead and
    /// removed, so those answers only cover that short window. Turning every
    /// reader into a fallible one would push a `Result` through render,
    /// detection and the API for a state that lasts under a second.
    pub core: Mutex<PaneTerminalCore>,
}

pub(crate) struct PaneTerminalCore {
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

impl PaneTerminal {
    pub(crate) fn on_next_dirty_collection(&self, hook: Box<dyn FnOnce() + Send>) {
        if let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) {
            core.dirty_collection_hook = Some(hook);
        }
    }

    /// Whether a panic while holding the core lock has broken the core. A
    /// single atomic load, taking no lock: the PTY actor asks on every loop.
    pub(crate) fn core_poisoned(&self) -> bool {
        shepr_vt::terminal_core_is_poisoned(&self.core)
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
        let Some(origin) = self.history_origin() else {
            return TerminalSearchWindow::empty();
        };
        let window = self.search_text_window_absolute(
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
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        let origin = core.terminal.history_origin();
        let target = word_motion_in(
            &core.terminal,
            absolute_point(TerminalTextPoint { row, col }, origin),
            motion,
        )?;
        Some(screen_point(target, origin))
    }

    pub(crate) fn dimensions(&self) -> Option<(u16, u16)> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        Some((core.terminal.cols(), core.terminal.rows()))
    }

    /// Paragraph motion with screen rows; see
    /// [`PaneTerminal::paragraph_motion_target_absolute`].
    pub(crate) fn paragraph_motion_target(
        &self,
        row: ScreenRow,
        direction: i8,
    ) -> Option<TerminalTextPoint> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        let origin = core.terminal.history_origin();
        let absolute = row.absolute(origin);
        let target = paragraph_motion_in(&core.terminal, absolute, direction)?;
        Some(screen_point(target, origin))
    }

    pub(crate) fn keyboard_protocol(
        &self,
        fallback: shepr_termio::input::KeyboardProtocol,
    ) -> shepr_termio::input::KeyboardProtocol {
        self.negotiated_keyboard_protocol().unwrap_or(fallback)
    }

    /// Where a copy-mode word motion from `row`/`col` lands, with absolute
    /// rows. `None` when the row is no longer retained.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the absolute-row reader is retained for focused tests"
        )
    )]
    pub(crate) fn word_motion_target_absolute(
        &self,
        row: AbsRow,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint<AbsRow>> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        word_motion_in(&core.terminal, TerminalTextPoint { row, col }, motion)
    }

    /// The next blank row above (`direction < 0`) or below `row`, with
    /// absolute rows, looking at most 1000 rows away. `None` when the row is
    /// no longer retained.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the absolute-row reader is retained for focused tests"
        )
    )]
    pub(crate) fn paragraph_motion_target_absolute(
        &self,
        row: AbsRow,
        direction: i8,
    ) -> Option<TerminalTextPoint<AbsRow>> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        paragraph_motion_in(&core.terminal, row, direction)
    }
}

mod backend;
mod helpers;
#[cfg(test)]
mod tests;
mod text;

use helpers::*;
use text::*;
