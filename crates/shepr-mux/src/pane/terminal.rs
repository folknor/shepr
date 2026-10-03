use crate::limits::{
    COPY_MODE_WORD_SEPARATORS, DEFAULT_DETECTION_ROWS, MERGE_MAX_BYTES, MERGE_MAX_ROWS,
    SCAN_CHUNK_ROWS, SYNCHRONIZED_OUTPUT_FLUSH_MARGIN,
};
pub use shepr_term::ScrollMetrics;
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
pub type TerminalTextPoint = Point<AbsRow>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTextRange {
    pub start: TerminalTextPoint,
    pub end: TerminalTextPoint,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSearchCase {
    Sensitive,
    Insensitive,
    /// Search case-sensitively only when the query contains an uppercase letter.
    Smart,
}

impl TerminalSearchCase {
    fn is_sensitive(self, query: &str) -> bool {
        match self {
            Self::Sensitive => true,
            Self::Insensitive => false,
            Self::Smart => query.chars().any(char::is_uppercase),
        }
    }
}

/// The maximum number of matches to keep around the current search result.
/// Zero disables the search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSearchLimit(usize);

impl TerminalSearchLimit {
    pub const fn new(limit: usize) -> Self {
        Self(limit)
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

/// One text search request. Keeping its cursor, continuation match and window
/// limit together prevents callers from swapping unrelated primitive values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTextSearch<'a> {
    pub query: &'a str,
    pub case: TerminalSearchCase,
    pub direction: TerminalSearchDirection,
    pub cursor: TerminalTextPoint,
    pub previous: Option<TerminalTextRange>,
    pub limit: TerminalSearchLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSearchPosition {
    /// The match's index in the returned window.
    pub window_index: usize,
    /// The match's index in the full result set.
    pub global_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSearchWindow {
    pub matches: Vec<TerminalTextMatch>,
    /// Absent when the search found no matches; both indexes travel together.
    pub current: Option<TerminalSearchPosition>,
    pub total: usize,
}

impl TerminalSearchWindow {
    fn empty() -> Self {
        Self {
            matches: Vec::new(),
            current: None,
            total: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalLineMotion {
    End,
    FirstNonBlank,
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
pub enum TerminalParagraphMotion {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCopyMotion {
    Line(TerminalLineMotion),
    Word(TerminalWordMotion),
    Paragraph(TerminalParagraphMotion),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCopyMotionError {
    RowUnavailable,
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

#[derive(Debug, Clone, Copy)]
enum TerminalMutation {
    HostThemeUpdate,
    HostAppearanceUpdate,
    HostThemeRestore,
    AgentOscStateClear,
    DefaultColorOwnerUpdate,
    HistorySeed,
    Resize,
    DirtyCollectionHookUpdate,
    ScrollUp,
    ScrollDown,
    ScrollReset,
    SetScrollOffset,
}

/// The emulator mutex never exposes a guard after an interrupted mutation.
pub(crate) struct TerminalCore(Mutex<PaneTerminalCore>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCorePoisoned;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalCoreTryLockError {
    WouldBlock,
    Poisoned,
}

impl TerminalCore {
    fn new(core: PaneTerminalCore) -> Self {
        Self(Mutex::new(core))
    }

    pub(crate) fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, PaneTerminalCore>, TerminalCorePoisoned> {
        self.0.lock().map_err(|_| TerminalCorePoisoned)
    }

    pub(crate) fn try_lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, PaneTerminalCore>, TerminalCoreTryLockError> {
        match self.0.try_lock() {
            Ok(guard) => Ok(guard),
            Err(std::sync::TryLockError::WouldBlock) => Err(TerminalCoreTryLockError::WouldBlock),
            Err(std::sync::TryLockError::Poisoned(_)) => Err(TerminalCoreTryLockError::Poisoned),
        }
    }

    fn is_poisoned(&self) -> bool {
        self.0.is_poisoned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchRow {
    pub y: u16,
    pub cells: Vec<CellData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalDirtyPatch {
    pub rows: Vec<PatchRow>,
}

/// A dirty patch with the revision and metadata read in the same terminal-core
/// hold.
pub struct TerminalDirtyPatchSnapshot {
    /// `None` means the terminal is clean. Unavailable reads return an error.
    pub patch: Option<TerminalDirtyPatch>,
    pub content_revision: u64,
    pub scroll_metrics: ScrollMetrics,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub alternate_screen_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchFallback {
    HyperlinkPresent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchUnavailable {
    CorePoisoned,
    SynchronizedOutput,
    Fallback(PatchFallback),
}

pub(super) type TerminalDirtyPatchCollection = Result<Option<TerminalDirtyPatch>, PatchFallback>;

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

pub(crate) type ProcessBytesResult =
    Result<ProcessBytesEffects, crate::pane::terminal::TerminalCorePoisoned>;

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
    pub core: TerminalCore,
    pub render_queued: std::sync::Arc<AtomicBool>,
    /// Set when parsing output (a read, or a synchronized update flushed by
    /// its timeout) leaves the terminal on the other screen than before, and
    /// cleared by the server once it has re-applied the pane's workspace
    /// geometry (pane chrome differs between the screens). The parse path
    /// compares the active screen once per read, not per byte, and stores
    /// only on an actual flip; the server reads it without the core lock.
    screen_flipped: AtomicBool,
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
    /// Stored revisions are even. A full surface spans several core holds;
    /// the server marks its revision odd if output changed during drawing.
    /// A retained patch reads its cells and revision under one hold.
    pub content_revision: u64,
    /// Live output, completed synchronized updates, clears and resizes only.
    /// Viewport and host presentation changes do not invalidate screen scans.
    pub detection_content_seq: u64,
    /// Runs during the next dirty-patch collection attempt, even if it falls
    /// back; see
    /// `PaneRuntime::on_next_dirty_collection`.
    pub dirty_collection_hook: Option<Box<dyn FnOnce() + Send>>,
    pub terminal: shepr_vt::Terminal,
    // This is an equality token paired with the active flag under one hold.
    // Poison is represented by None in synchronized_output_state, never by
    // an invented epoch. Keeping that read atomic is the important invariant.
    synchronized_output_epoch: u64,
    /// Bumped by every resize that changes the grid. A taller grid pulls
    /// history rows back onto the screen, where the child can rewrite them,
    /// so history formatted before a resize may no longer match its rows
    /// (`history.rs`).
    history_epoch: u64,
    pub render_state: shepr_vt::RenderState,
    pub host_terminal_theme: shepr_term::host::TerminalTheme,
    /// Process group of the foreground program that last overrode a default
    /// colour (OSC 10/11); its overrides are dropped once the shell is back
    /// in the foreground. `None` while no override is in effect.
    pub transient_default_color_owner_pgid: Option<shepr_platform::Pgid>,
    // Raw only inside the core; effects carry DefaultColorGeneration so an
    // owner probe cannot be mistaken for another kind of generation.
    default_color_generation: u64,
    pub(super) osc_debug_tracker: OscDebugTracker,
    pub(super) agent_osc_state: AgentOscStateTracker,
}

/// Record the meaning of a mutation once, rather than choosing counters at
/// every parser, timer and presentation call site. These counters stay raw at
/// the detection and persistence boundaries, which consume equality tokens.
#[derive(Clone, Copy)]
#[expect(
    variant_size_differences,
    reason = "a short-lived value passed by copy; the largest variant is one slice and two flags"
)]
enum CoreMutation<'a> {
    Output {
        bytes: &'a [u8],
        flushed: bool,
        sync_changed: bool,
    },
    SyncFlush,
    Resize {
        grid_changed: bool,
        sync_changed: bool,
    },
    Presentation,
    Clear,
}

impl PaneTerminalCore {
    fn record_mutation(&mut self, mutation: CoreMutation<'_>) {
        self.content_revision = self.content_revision.wrapping_add(2);
        let (detection_changed, sync_changes, grid_changed) = match mutation {
            CoreMutation::Output {
                bytes,
                flushed,
                sync_changed,
            } => {
                super::agent_detection::observe_detection_content_change(
                    bytes,
                    &mut self.detection_content_seq,
                );
                (flushed, u64::from(flushed) + u64::from(sync_changed), false)
            }
            CoreMutation::SyncFlush => (true, 1, false),
            CoreMutation::Resize {
                grid_changed,
                sync_changed,
            } => (true, u64::from(sync_changed), grid_changed),
            CoreMutation::Clear => (true, 0, false),
            CoreMutation::Presentation => (false, 0, false),
        };
        if detection_changed {
            super::agent_detection::mark_detection_content_changed(&mut self.detection_content_seq);
        }
        self.synchronized_output_epoch = self.synchronized_output_epoch.wrapping_add(sync_changes);
        if grid_changed {
            self.history_epoch = self.history_epoch.wrapping_add(1);
        }
    }
}

impl PaneTerminal {
    /// Records an active-screen flip across one parse. Relaxed is enough: the
    /// render wake the same parse raises orders it before the server's read.
    fn note_screen_flip(&self, before: shepr_vt::ActiveScreen, after: shepr_vt::ActiveScreen) {
        if before != after {
            self.screen_flipped.store(true, Ordering::Relaxed);
        }
    }

    /// Whether output flipped the active screen since the flag was last taken.
    pub(crate) fn screen_flip_pending(&self) -> bool {
        self.screen_flipped.load(Ordering::Relaxed)
    }

    /// Takes the screen-flip flag; see `screen_flipped`.
    pub(crate) fn take_screen_flip(&self) -> bool {
        self.screen_flipped.swap(false, Ordering::Relaxed)
    }

    fn report_terminal_mutation_failure(&self, operation: TerminalMutation) {
        if !self.mutation_failure_reported.swap(true, Ordering::Relaxed) {
            if let Some(pane_id) = self.pane_id {
                error!(
                    pane = pane_id.raw(),
                    ?operation,
                    "terminal core lock poisoned; mutation was not applied"
                );
            } else {
                error!(
                    ?operation,
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
    fn report_dirty_patch_fallback(&self, reason: PatchUnavailable) {
        if !self
            .dirty_patch_fallback_reported
            .swap(true, Ordering::Relaxed)
        {
            debug!(?reason, "dirty terminal patch fell back to a full render");
        }
    }

    pub(crate) fn on_next_dirty_collection(&self, hook: Box<dyn FnOnce() + Send>) {
        let Ok(mut core) = self.core.lock() else {
            self.report_terminal_mutation_failure(TerminalMutation::DirtyCollectionHookUpdate);
            return;
        };
        core.dirty_collection_hook = Some(hook);
    }

    /// Whether a panic while holding the core lock has broken the core. A
    /// single atomic load, taking no lock: the PTY actor asks on every loop.
    pub(crate) fn core_poisoned(&self) -> bool {
        self.core.is_poisoned()
    }

    pub(crate) fn dimensions(&self) -> Option<shepr_core::geometry::GridSize> {
        let core = self.core.lock().ok()?;
        Some(shepr_core::geometry::GridSize::clamped(
            core.terminal.cols(),
            core.terminal.rows(),
        ))
    }

    pub(crate) fn keyboard_protocol(
        &self,
        fallback: shepr_term::key::KeyboardProtocol,
    ) -> shepr_term::key::KeyboardProtocol {
        self.negotiated_keyboard_protocol().unwrap_or(fallback)
    }

    /// Where a copy-mode word motion from the cursor lands. `None` when its
    /// row is no longer retained.
    pub(crate) fn word_motion_target(
        &self,
        cursor: TerminalTextPoint,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint> {
        let core = self.core.lock().ok()?;
        word_motion_in(&core.terminal, cursor, motion)
    }

    /// The next blank row above or below the cursor, looking at most 1000 rows
    /// away. `None` when its row is no longer retained.
    pub(crate) fn paragraph_motion_target(
        &self,
        cursor: TerminalTextPoint,
        motion: TerminalParagraphMotion,
    ) -> Option<TerminalTextPoint> {
        let core = self.core.lock().ok()?;
        paragraph_motion_in(&core.terminal, cursor, motion)
    }
}

mod backend;
mod helpers;
mod history;
mod input;
mod text;

pub use history::{HistoryPiece, HistoryUnavailable, PaneHistoryCache, PaneHistorySource};
pub use input::WheelRouting;

use helpers::*;
use text::*;

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TerminalDirtyPatchOutcome {
    Clean,
    Patch(TerminalDirtyPatch),
    Fallback,
}

#[cfg(test)]
mod invariant_tests;

#[cfg(test)]
mod tests;
