use crate::limits::{
    COPY_MODE_WORD_SEPARATORS, DEFAULT_DETECTION_ROWS, MERGE_MAX_BYTES, MERGE_MAX_ROWS,
    SCAN_CHUNK_ROWS, SYNCHRONIZED_OUTPUT_FLUSH_MARGIN,
};
pub use shepr_termio::ScrollMetrics;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ratatui::layout::Rect;
use tracing::{debug, error, warn};
use unicode_width::UnicodeWidthStr;

use shepr_core::layout::PaneId;
use shepr_protocol::{CellData, FrameData, GridCellWidth, WireColor, WireStyle, WireStyleFlags};
use shepr_vt::{AbsRow, Point, ScreenRow, ViewportRow};

use super::cursor::decscusr_cursor_shape;
use super::osc::{
    AgentOscStateTracker, OscDebugTracker, current_transient_default_color_owner,
    parse_reported_cwd, restore_host_terminal_theme_if_needed,
};

/// A cell position in terminal text, on a stable absolute row: output and
/// history eviction never make it name another line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TerminalTextPoint {
    pub row: AbsRow,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTextMatch {
    pub start: TerminalTextPoint,
    pub end: TerminalTextPoint,
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
pub struct TerminalSearchWindow {
    pub matches: Vec<TerminalTextMatch>,
    pub current: Option<usize>,
    pub current_global: Option<usize>,
    pub total: usize,
}

impl TerminalSearchWindow {
    fn empty() -> Self {
        Self {
            matches: Vec::new(),
            current: None,
            current_global: None,
            total: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalWordMotion {
    NextStart,
    PreviousStart,
    NextEnd,
    NextBigStart,
    PreviousBigStart,
    NextBigEnd,
}

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
    AlternateScreenActive,
}

impl std::fmt::Display for PaneClearError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TerminalLockPoisoned => f.write_str("terminal lock poisoned"),
            Self::AlternateScreenActive => f.write_str("the pane is on the alternate screen"),
        }
    }
}

impl std::error::Error for PaneClearError {}

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

pub(super) struct TerminalDirtyPatchCollection {
    pub outcome: TerminalDirtyPatchOutcome,
    pub fallback_reason: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderRequest {
    /// No render is due yet.
    None,
    /// Render as soon as the read's immediate effects are applied.
    Now,
    /// Render when the synchronized-output timeout flushes the frame.
    After(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DefaultColorGeneration(pub(crate) u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessBytesEffects {
    pub render_request: RenderRequest,
    pub terminal_title_changed: bool,
    pub clipboard_writes: Vec<Vec<u8>>,
    pub reported_cwd: Option<std::path::PathBuf>,
    pub terminal_responses: Vec<Bytes>,
    pub default_color_generation: Option<DefaultColorGeneration>,
}

pub(crate) type ProcessBytesResult = Result<ProcessBytesEffects, shepr_vt::TerminalCorePoisoned>;

pub(crate) struct PaneTerminal {
    /// Poisoned for good once anything panics while holding it. The readers
    /// below then answer empty or default values rather than error: the PTY
    /// actor checks `is_poisoned` on every loop (idle polls included, so at
    /// least once a second) and ends the pane, which is reported dead and
    /// removed, so those answers only cover that short window. Turning every
    /// reader into a fallible one would push a `Result` through render,
    /// detection and the API for a state that lasts under a second. Writers do
    /// not treat a poisoned lock as a successful mutation: operations without
    /// a failure return log their skipped operation once per pane.
    pub core: Mutex<PaneTerminalCore>,
    pub render_queued: std::sync::Arc<AtomicBool>,
    /// Set on production construction so mutations without a pane-id
    /// argument can identify their owner in a failure report.
    pane_id: Option<PaneId>,
    mutation_failure_reported: AtomicBool,
    oversized_clipboard_reported: AtomicBool,
    dirty_patch_fallback_reported: AtomicBool,
}

/// What agent screen detection evaluates for one pane, read at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentDetectionInputs {
    pub screen_text: String,
    pub osc_title: String,
    pub osc_progress: String,
}

pub(crate) struct PaneTerminalCore {
    /// Render-visible mutations advance this while holding the core lock.
    /// Revisions remain even for callers that previously tested write parity;
    /// exclusion is now provided by the core, with no announced-write state.
    pub content_revision: u64,
    /// Live output, completed synchronized updates, clears and resizes only.
    /// Viewport and host presentation changes do not invalidate screen scans.
    pub detection_content_seq: u64,
    /// Runs during the next dirty-patch collection attempt, even if it falls
    /// back; see
    /// `PaneRuntime::on_next_dirty_collection`.
    pub dirty_collection_hook: Option<Box<dyn FnOnce() + Send>>,
    pub terminal: shepr_vt::Terminal,
    synchronized_output_epoch: u64,
    /// Bumped by every resize that changes the grid. A taller grid pulls
    /// history rows back onto the screen, where the child can rewrite them,
    /// so history formatted before a resize may no longer match its rows
    /// (`history.rs`).
    history_epoch: u64,
    pub render_state: shepr_vt::RenderState,
    pub initial_default_foreground: shepr_vt::RgbColor,
    pub initial_default_background: shepr_vt::RgbColor,
    pub host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
    /// Process group of the foreground program that last overrode a default
    /// colour (OSC 10/11); its overrides are dropped once the shell is back
    /// in the foreground. `None` while no override is in effect.
    pub transient_default_color_owner_pgid: Option<u32>,
    default_color_generation: u64,
    pub(super) osc_debug_tracker: OscDebugTracker,
    pub(super) agent_osc_state: AgentOscStateTracker,
}

impl PaneTerminal {
    fn report_terminal_mutation_failure(&self, operation: &'static str) {
        if !self.mutation_failure_reported.swap(true, Ordering::Relaxed) {
            if let Some(pane_id) = self.pane_id {
                error!(
                    pane = pane_id.raw(),
                    operation, "terminal core lock poisoned; mutation was not applied"
                );
            } else {
                error!(
                    operation,
                    "terminal core lock poisoned; mutation was not applied"
                );
            }
        }
    }

    fn report_oversized_clipboard_store(&self, pane_id: PaneId, bytes: usize) {
        if !self
            .oversized_clipboard_reported
            .swap(true, Ordering::Relaxed)
        {
            warn!(
                pane = pane_id.raw(),
                bytes, "dropped oversized OSC 52 clipboard store"
            );
        }
    }

    /// A fallback is routine (a visible hyperlink is enough), so this is
    /// diagnostic detail, recorded once per pane.
    fn report_dirty_patch_fallback(&self, reason: &'static str) {
        if !self
            .dirty_patch_fallback_reported
            .swap(true, Ordering::Relaxed)
        {
            debug!(reason, "dirty terminal patch fell back to a full render");
        }
    }

    pub(crate) fn on_next_dirty_collection(&self, hook: Box<dyn FnOnce() + Send>) {
        let Ok(mut core) = shepr_vt::lock_terminal_core(&self.core) else {
            self.report_terminal_mutation_failure("dirty collection hook update");
            return;
        };
        core.dirty_collection_hook = Some(hook);
    }

    /// Whether a panic while holding the core lock has broken the core. A
    /// single atomic load, taking no lock: the PTY actor asks on every loop.
    pub(crate) fn core_poisoned(&self) -> bool {
        shepr_vt::terminal_core_is_poisoned(&self.core)
    }

    pub(crate) fn dimensions(&self) -> Option<(u16, u16)> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        Some((core.terminal.cols(), core.terminal.rows()))
    }

    pub(crate) fn keyboard_protocol(
        &self,
        fallback: shepr_termio::input::KeyboardProtocol,
    ) -> shepr_termio::input::KeyboardProtocol {
        self.negotiated_keyboard_protocol().unwrap_or(fallback)
    }

    /// Where a copy-mode word motion from `row`/`col` lands. `None` when the
    /// row is no longer retained.
    pub(crate) fn word_motion_target(
        &self,
        row: AbsRow,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        word_motion_in(&core.terminal, TerminalTextPoint { row, col }, motion)
    }

    /// The next blank row above (`direction < 0`) or below `row`, looking at
    /// most 1000 rows away. `None` when the row is no longer retained.
    pub(crate) fn paragraph_motion_target(
        &self,
        row: AbsRow,
        direction: i8,
    ) -> Option<TerminalTextPoint> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        paragraph_motion_in(&core.terminal, row, direction)
    }
}

mod backend;
mod helpers;
mod history;
mod text;

pub use history::{HistoryPiece, PaneHistoryCache, PaneHistorySource};

use helpers::*;
use text::*;

#[cfg(test)]
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod invariant_tests;

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

#[cfg(test)]
mod tests;
