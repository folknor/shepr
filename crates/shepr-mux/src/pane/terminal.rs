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
use tracing::debug;

use shepr_core::layout::PaneId;
use shepr_protocol::{CellData, FrameData, GridCellWidth, WireColor, WireStyle, WireStyleFlags};
use shepr_vt::{AbsRow, Point, ScreenRow};

use super::agent_osc::AgentOscStateTracker;
use super::cursor::decscusr_cursor_shape;
use super::osc_debug::{self, OscDebugEvent};
use super::osc7::parse_reported_cwd;

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

pub use shepr_term::copy_motion::CopyMotion as TerminalCopyMotion;
pub use shepr_term::copy_motion::LineMotion as TerminalLineMotion;
pub use shepr_term::copy_motion::ParagraphMotion as TerminalParagraphMotion;
pub use shepr_term::copy_motion::SearchDirection as TerminalSearchDirection;
pub use shepr_term::copy_motion::WordMotion as TerminalWordMotion;

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
    /// Whether the scan reached the end of the text. The chunked live search
    /// stops early when the terminal re-wraps or switches screens between
    /// chunks, and then `total` counts only the rows it scanned.
    pub complete: bool,
}

impl TerminalSearchWindow {
    fn empty() -> Self {
        Self {
            matches: Vec::new(),
            current: None,
            total: 0,
            complete: true,
        }
    }
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
    pub content_revision: ContentRevision,
    pub scroll_metrics: ScrollMetrics,
    pub mouse_reporting: bool,
    pub pixel_mouse: shepr_term::mouse::PanePixelMouse,
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
    /// Set when parsing output (a read, or a synchronized update flushed by
    /// its timeout) leaves the terminal on the other screen than before, and
    /// cleared by the server once it has re-applied the pane's workspace
    /// geometry (pane chrome differs between the screens). The parse path
    /// compares the active screen once per read, not per byte, and stores
    /// only on an actual flip; the server reads it without the core lock.
    screen_flipped: AtomicBool,
    /// Whether a synchronized update is open, as of the last mutation recorded
    /// through `commit_mutation`. Written under the core lock, read without it;
    /// a stale read at worst sends the caller on to the locked check.
    synchronized_output: AtomicBool,
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
    /// The latest OSC 0/2 title, `None` when none was seen or it was cleared.
    pub osc_title: Option<String>,
    /// The latest OSC 9;4 report in its canonical `4;state[;percent]`
    /// spelling, `None` when none was seen.
    pub osc_progress: Option<String>,
}

pub(crate) struct PaneTerminalCore {
    /// Every counter below advances only in `record_mutation` (the default
    /// colour generation in `note_default_color_change`, which belongs to the
    /// same effects pass), while holding the core lock. Callers outside that
    /// pass reach it through `PaneTerminal::commit_mutation`, which also
    /// refreshes the lock-free synchronized-output mirror.
    content_revision: ContentRevision,
    detection_seq: DetectionSeq,
    /// Runs during the next dirty-patch collection attempt, even if it falls
    /// back; see
    /// `PaneRuntime::on_next_dirty_collection`.
    dirty_collection_hook: Option<Box<dyn FnOnce() + Send>>,
    pub(super) terminal: shepr_vt::Terminal,
    // Paired with the active flag under one hold; see `SyncState`. Poison is
    // `SyncState::Poisoned`, never an invented epoch. Keeping that read atomic
    // is the important invariant.
    synchronized_output_epoch: SyncEpoch,
    pub(super) render_state: shepr_vt::RenderState,
    pub(super) host_terminal_theme: shepr_term::host::TerminalTheme,
    /// Process group of the foreground program that last overrode a default
    /// colour (OSC 10/11); its overrides are dropped once the shell is back
    /// in the foreground. `None` while no override is in effect.
    transient_default_color_owner_pgid: Option<shepr_platform::Pgid>,
    default_color_generation: DefaultColorGeneration,
    pub(super) agent_osc_state: AgentOscStateTracker,
    /// The server's host names as resolved at its startup, `None` when they
    /// could not be: OSC 7 `file://` reports naming either are this machine's.
    /// Shared by every pane, so construction clones only the `Arc`.
    pub(super) local_host: Option<std::sync::Arc<shepr_platform::HostNames>>,
    /// Whether the server last told this pane it holds terminal focus,
    /// recorded whether or not the child had focus reporting on. A child
    /// that turns reporting on while the pane holds focus is told focus-in
    /// at once (`focus_report_on_enable`): it cannot have heard of a focus
    /// gained before it asked, and a fresh shell, such as the one an agent
    /// resume launches, never has reporting on when it is told.
    pub(super) pane_focused: bool,
}

/// Record the meaning of a mutation once, rather than choosing counters at
/// every parser, timer and presentation call site. The counters are typed
/// (`counters.rs`); the detection and persistence boundaries read them through
/// accessors that hand out only equality tokens.
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
        sync_changed: bool,
    },
    Presentation,
    Clear,
    /// The child set a default colour (OSC 10/11). Advances only the
    /// generation an owner probe is checked against: the output or flush that
    /// carried it is recorded as its own mutation.
    DefaultColorSet,
}

impl PaneTerminalCore {
    fn record_mutation(&mut self, mutation: CoreMutation<'_>) {
        // How far each counter moves, besides the content revision, which
        // every other mutation advances once.
        let (detection_bumps, sync_steps) = match mutation {
            CoreMutation::DefaultColorSet => {
                self.default_color_generation.advance();
                return;
            }
            CoreMutation::Output {
                bytes,
                flushed,
                sync_changed,
            } => (
                u8::from(!bytes.is_empty()) + u8::from(flushed),
                u8::from(flushed) + u8::from(sync_changed),
            ),
            CoreMutation::SyncFlush => (1, 1),
            CoreMutation::Resize { sync_changed } => (1, u8::from(sync_changed)),
            CoreMutation::Clear => (1, 0),
            CoreMutation::Presentation => (0, 0),
        };
        self.content_revision.advance();
        for _ in 0..detection_bumps {
            self.detection_seq.bump();
        }
        self.synchronized_output_epoch.advance(sync_steps);
    }

    /// The detector's token for this core's screen content.
    pub(super) fn detection_seq(&self) -> DetectionSeq {
        self.detection_seq
    }

    fn sync_state(&self) -> SyncState {
        if self.terminal.sync_update_buffering() {
            SyncState::Active
        } else {
            SyncState::Idle(self.synchronized_output_epoch)
        }
    }

    /// The foreground group whose default-colour overrides are due for a
    /// restore check: one is recorded, the host theme is known to restore to,
    /// and the main screen is showing (the alternate screen defers the
    /// restore). The process half of the policy lives on the runtime side.
    fn theme_restore_owner(&self) -> Option<shepr_platform::Pgid> {
        let owner = self.transient_default_color_owner_pgid?;
        if self.host_terminal_theme.is_empty()
            || self.terminal.active_screen() == shepr_vt::ActiveScreen::Alternate
        {
            return None;
        }
        Some(owner)
    }
}

/// The result of drawing one pane's screen, decided in the same terminal-core
/// hold as the cells: a caller never pre-checks synchronized output and then
/// draws, because the two could disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneDraw {
    /// The screen was drawn. The stamp names the state it was drawn from, read
    /// in the same hold: the synchronized-output epoch and the content
    /// revision. Later reads of the pane's metadata are compared against it.
    Drawn {
        sync_epoch: SyncEpoch,
        content_revision: ContentRevision,
    },
    /// A synchronized update is open: nothing was drawn, and the frame is not
    /// drawable until the update ends.
    Deferred,
    /// The core lock is poisoned: nothing was drawn. The PTY actor closes the
    /// pane shortly.
    Unreadable,
}

/// The pane's cursor read in one terminal-core hold, with the synchronized
/// output gate decided in that hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorRead {
    Shown(TerminalCursorState),
    /// A synchronized update is open: the cursor is not drawable.
    Deferred,
    /// The core is unreadable or has no cursor to report.
    Unavailable,
}

impl PaneTerminal {
    /// Records a core mutation and refreshes the lock-free synchronized-output
    /// mirror in the same hold. Every mutation recorded under the core lock
    /// goes through here, so the mirror cannot miss one.
    fn commit_mutation(&self, core: &mut PaneTerminalCore, mutation: CoreMutation<'_>) {
        core.record_mutation(mutation);
        self.mirror_synchronized_output(core);
    }

    /// Stores whether a synchronized update is open, for readers that must not
    /// take the core lock. Called with the core lock held.
    fn mirror_synchronized_output(&self, core: &PaneTerminalCore) {
        self.synchronized_output
            .store(core.terminal.sync_update_buffering(), Ordering::Release);
    }

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
                shepr_platform::structured_log!(
                    ERROR, event = terminal.mutation, outcome = Poisoned,
                    pane = %pane_id,
                    ?operation,
                    "terminal core lock poisoned; mutation was not applied"
                );
            } else {
                shepr_platform::structured_log!(
                    ERROR,
                    event = terminal.mutation,
                    outcome = Poisoned,
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
            shepr_platform::structured_log!(
                WARN, event = clipboard.osc_store, outcome = Oversized,
                pane = %pane_id,
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

    /// The detector's token for the screen content; `None` when the core is
    /// poisoned.
    pub(crate) fn detection_seq(&self) -> Option<DetectionSeq> {
        Some(self.core.lock().ok()?.detection_seq())
    }

    /// The render revision, for a reader that compares it across holds.
    /// `None` when the core is poisoned.
    pub(crate) fn content_revision(&self) -> Option<ContentRevision> {
        Some(self.core.lock().ok()?.content_revision)
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

    /// Where a copy-mode word motion from the cursor lands, looking at most
    /// `MAX_WORD_MOTION_ROWS` rows away. `None` when its row is no longer
    /// retained or no target lies within reach.
    pub(crate) fn word_motion_target(
        &self,
        cursor: TerminalTextPoint,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint> {
        let core = self.core.lock().ok()?;
        word_motion_in(&core.terminal, cursor, motion)
    }

    /// The next blank row above or below the cursor, looking at most
    /// `MAX_PARAGRAPH_MOTION_ROWS` rows away. `None` when its row is no longer
    /// retained.
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
mod counters;
mod helpers;
mod input;
mod text;

pub(crate) use counters::DefaultColorGeneration;
pub use counters::{ContentRevision, DetectionSeq, SyncEpoch, SyncState};
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
