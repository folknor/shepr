//! Shepr's terminal-core boundary.
//!
//! The emulator underneath is `alacritty_terminal` (pinned in Cargo.toml). The
//! rest of the tree only sees the types in this module; alacritty types stay
//! private here. The adapter is contained in `crates/shepr-vt/src/`.
//!
//! What this adapter adds on top of alacritty:
//! * render snapshots with row dirty flags derived from alacritty's damage
//!   (`RenderState` and borrowed row/cell views);
//! * plain and VT formatters for reads and history persistence (`format.rs`);
//! * a `Handler` wrapper the parser drives in place of `Term` (`handler.rs`):
//!   it caps the kitty keyboard-mode stack before alacritty's broken overflow
//!   branch can panic, models modes 9/1016/2031/2048 and modifyOtherKeys,
//!   and gives the halfwidth voiced marks U+FF9E/U+FF9F (zero-width to
//!   unicode-width, and so to alacritty) a column of their own;
//! * a byte scanner for sequences vte never hands to a `Handler` at all
//!   (OSC 7 / 9;9 / 1337 CurrentDir, OSC 9;4 progress, CSI ? 996 n, CSI 16 t,
//!   XTGETTCAP, CSI ? 3 J, and the modifyOtherKeys spellings vte drops;
//!   `scan.rs`);
//! * query replies in byte order, with OSC colour queries surfaced as
//!   structured [`ColorQuery`] values so the pane can pick the reply form.
//!   Replies from the scanner are the exception inside a synchronized
//!   update: see [`Terminal::write`];
//! * host default colours ([`Terminal::set_default_colors`]) layered under
//!   the child's OSC 10/11 overrides. Host state is never written into the
//!   child's byte stream: that would share the parser, the scanner and
//!   vte's sync buffer with the child and could split its sequences;
//! * the window title, taken from alacritty's own `Title`/`ResetTitle`
//!   events (so OSC 0/2, the CSI 22/23 t title stack and RIS all count);
//! * byte-denominated scrollback limits converted to line counts;
//! * absolute row ids that stay attached to their lines while history is
//!   trimmed ([`Terminal::history_origin`], `rows.rs`);
//! * synchronized-output (mode 2026) timeout flushing.

mod cell;
mod color;
mod coords;
mod format;
mod handler;
mod history;
mod limits;
mod locks;
mod modes;
mod read;
mod render;
mod rows;
mod scan;
pub mod selection;
pub use cell::RenderColors;
pub use cell::{
    CellBasicData, CellColor, CellStyle, CellView, CellWide, UnderlineStyle,
    is_halfwidth_katakana_voiced_grapheme, is_halfwidth_katakana_voiced_mark,
    unicode_codepoint_width, unicode_text_width,
};
use cell::{CellText, cell_text, cell_text_into, cell_wide};
pub use cell::{RowWrap, unicode_display_units};
pub use format::AnsiCarry;
pub use modes::DecMode;
// limits-exempt: this fixed terminfo name advertises the pane terminal type.
pub const PANE_TERM: &str = "xterm-256color";
const PANE_TRUECOLOR_BITS_PER_CHANNEL: Option<&'static [u8]> = Some(b"8");
pub const PANE_COLORTERM: &str = match PANE_TRUECOLOR_BITS_PER_CHANNEL {
    Some(_) => "truecolor",
    None => "",
};

pub use color::{ColorQuery, ColorQueryTarget, DefaultColor, RgbColor, default_palette};
pub use render::{CursorVisualStyle, Dirty, RenderState};
pub use scan::{ProgressReport, WorkingDirectoryReport};

pub use locks::{
    TerminalCorePoisoned, lock_auxiliary, lock_terminal_core, recover_auxiliary_poison,
    terminal_core_is_poisoned,
};
pub use locks::{TerminalCoreTryLockError, try_lock_auxiliary, try_lock_terminal_core};

use std::cell::Cell as ClockCell;
use std::fmt;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermDamage, TermMode};
use unicode_width::UnicodeWidthChar;
use vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb, Timeout};

pub use coords::Point;
pub use coords::{AbsRow, ScreenRow, ViewportRow};

use self::format::Format;
use self::handler::{CoreHandler, KeyboardStackDepth};
use self::history::HistoryCapacity;
use self::rows::RowOrigin;
use self::scan::{ScanEvent, Scanner};
use crate::limits::{
    MAX_CLIPBOARD_BYTES, MAX_SCROLLBACK_LINES, MIN_SCROLLBACK_CELL_BYTES, MIN_SCROLLBACK_COLUMNS,
    MIN_SCROLLBACK_LINES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    RowNotRetained,
    ColumnOutOfRange,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowNotRetained => f.write_str("terminal row is not retained"),
            Self::ColumnOutOfRange => f.write_str("terminal column out of range"),
        }
    }
}

impl std::error::Error for ReadError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusEvent {
    Gained,
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}

impl ColorScheme {
    pub const fn report(self) -> &'static [u8] {
        match self {
            Self::Dark => b"\x1b[?997;1n",
            Self::Light => b"\x1b[?997;2n",
        }
    }
}

// Unicode private-use codepoint used by the kitty graphics unicode-placeholder
// convention. Shepr does not render kitty graphics, but programs may still
// emit this codepoint as literal text; keep filtering it out of copied,
// history and rendered text so stray placeholder glyphs don't leak into
// user-visible output.
// limits-exempt: the codepoint the kitty graphics protocol defines.
pub(crate) const KITTY_UNICODE_PLACEHOLDER: u32 = 0x10EEEE;

/// Fallback colours used until the program or host sets its own defaults.
/// The pane layer compares these with the initial render colours to detect
/// later default-colour overrides.
const DEFAULT_FOREGROUND: RgbColor = RgbColor {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};
const DEFAULT_BACKGROUND: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

// This parser boundary returns clipboard effects as data and has no logging
// dependency; oversize-store diagnostics belong with the pane-level consumer.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveScreen {
    Primary,
    Alternate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalScrollbar {
    pub total: usize,
    pub offset: usize,
    pub len: usize,
}

/// A reply the terminal wants written back to the child, in byte order.
#[derive(Debug)]
pub enum PtyResponse {
    Bytes(Vec<u8>),
    ColorQuery(ColorQuery),
}

pub fn encode_focus(event: FocusEvent) -> &'static [u8] {
    match event {
        FocusEvent::Gained => b"\x1b[I",
        FocusEvent::Lost => b"\x1b[O",
    }
}

fn scrollback_lines(max_scrollback_bytes: usize, columns: usize) -> usize {
    if max_scrollback_bytes == 0 {
        return 0;
    }
    let bytes_per_line = columns
        .max(MIN_SCROLLBACK_COLUMNS)
        .saturating_mul(mem::size_of::<Cell>())
        .max(MIN_SCROLLBACK_CELL_BYTES);
    (max_scrollback_bytes / bytes_per_line).clamp(MIN_SCROLLBACK_LINES, MAX_SCROLLBACK_LINES)
}

fn term_config(scrolling_history: usize) -> Config {
    Config {
        scrolling_history,
        kitty_keyboard: true,
        osc52: Osc52::OnlyCopy,
        ..Config::default()
    }
}

struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Events retained by the adapter, in emission order.
enum TerminalEvent {
    PtyWrite(Vec<u8>),
    ColorQuery(ColorQuery),
    ClipboardStore(ClipboardType, String),
    Title(String),
    ResetTitle,
}

/// Collects the alacritty events the adapter acts on, in emission order.
/// Bells are not among them: nothing in shepr surfaces a bell.
#[derive(Clone)]
struct Listener(Arc<Mutex<Vec<TerminalEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let event = match event {
            Event::PtyWrite(text) => Some(TerminalEvent::PtyWrite(text.into_bytes())),
            Event::ClipboardStore(clipboard, text) => {
                Some(TerminalEvent::ClipboardStore(clipboard, text))
            }
            Event::Title(title) => Some(TerminalEvent::Title(title)),
            Event::ResetTitle => Some(TerminalEvent::ResetTitle),
            _ => None,
        };
        if let Some(event) = event {
            crate::lock_auxiliary(&self.0).push(event);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ExtraModes {
    x10_mouse: bool,
    sgr_pixels_mouse: bool,
    color_scheme_report: bool,
    in_band_resize: bool,
    /// xterm modifyOtherKeys level (0, 1 or 2).
    modify_other_keys: ModifyOtherKeysLevel,
    /// The child chose a cursor shape (DECSCUSR 1-6 or OSC 50) and has not
    /// asked for the default back (DECSCUSR 0, RIS).
    cursor_shape_set: bool,
    /// Between vte's BSU and ESU (or timeout) as the handler sees them, which
    /// inside a buffered frame is replay order. Only DECRQM ?2026 reads it;
    /// [`Terminal::mode_get`] asks the parser.
    synchronized_update: bool,
}

/// The three xterm modifyOtherKeys levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModifyOtherKeysLevel {
    #[default]
    Off,
    ExceptWellDefined,
    All,
}

impl ModifyOtherKeysLevel {
    pub const fn from_parameter(value: u16) -> Self {
        match value {
            0 => Self::Off,
            1 => Self::ExceptWellDefined,
            _ => Self::All,
        }
    }

    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::ExceptWellDefined => 1,
            Self::All => 2,
        }
    }
}

impl fmt::Display for ModifyOtherKeysLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_u8())
    }
}

/// A parsed title event; absence of an event is represented by `None` at the
/// collection boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleUpdate {
    Set(String),
    Reset,
}

/// Effects collected from the terminal since its previous effect drain.
#[must_use = "terminal effects must be handled or explicitly discarded"]
pub struct TerminalEffects {
    /// Replies to write to the child in parser order.
    pub pty_responses: Vec<PtyResponse>,
    /// Working-directory reports observed in child output.
    pub pwd_changes: Vec<WorkingDirectoryReport>,
    /// Clipboard stores requested by the child.
    pub clipboard_writes: Vec<Vec<u8>>,
    /// Sizes of clipboard stores dropped for exceeding the configured limit.
    pub dropped_clipboard_store_bytes: Vec<usize>,
    /// The latest uncollected window-title change.
    pub title_update: Option<TitleUpdate>,
    /// The latest uncollected OSC 9;4 progress report.
    pub progress_update: Option<ProgressReport>,
    /// Whether the child set a default foreground or background since the drain.
    pub default_color_set: bool,
}

/// Result of a host-requested clear operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearScreenOutcome {
    Cleared,
    AlternateScreenActive,
}

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor<SyncUpdateTimeout>,
    /// Mirror of alacritty's keyboard-mode stack depths; the parser must only
    /// ever drive `term` through a [`CoreHandler`] so it stays exact.
    keyboard_depth: KeyboardStackDepth,
    events: Arc<Mutex<Vec<TerminalEvent>>>,
    scanner: Scanner,
    max_scrollback: usize,
    /// History capacity in lines. It grows with the resize budget, decreases
    /// only after a primary-screen width change (never below the content held)
    /// or an explicit primary-history purge, and never on a height change.
    history_lines: usize,
    default_palette: [RgbColor; shepr_core::limits::PALETTE_COLOR_COUNT],
    cell: Option<shepr_core::geometry::CellPx>,
    modes: ExtraModes,
    color_scheme: Option<ColorScheme>,
    /// The host's default foreground/background. They sit under the child's
    /// OSC 10/11 overrides (alacritty's `colors` slots) and over the built-in
    /// defaults, so host theme changes never go through the child's parser.
    host_foreground: Option<RgbColor>,
    host_background: Option<RgbColor>,
    responses: Vec<PtyResponse>,
    pwd_changes: Vec<WorkingDirectoryReport>,
    clipboard_writes: Vec<Vec<u8>>,
    dropped_clipboard_store_bytes: Vec<usize>,
    /// The latest title change not yet collected.
    title_update: Option<TitleUpdate>,
    /// The latest OSC 9;4 progress payload (after `9;`) not yet collected.
    progress_update: Option<ProgressReport>,
    /// The child set the default foreground or background since the last
    /// [`Terminal::take_effects`].
    default_color_set: bool,
    /// Monotonic damage counter; [`RenderState`] remembers the last value it saw.
    damage_generation: u64,
    /// Generation of the most recent whole-viewport damage.
    full_damage_generation: u64,
    /// Per viewport row: generation of the most recent damage to that row.
    row_damage_generations: Vec<u64>,
    /// Absolute row accounting (`rows.rs`): lines evicted from the top of
    /// the primary screen, so `origin + screen row` never shifts.
    rows: RowOrigin,
}

/// VTE calls `set_timeout` while parsing BSU, but its default handler reads
/// the process clock there. The caller sets `now` before each parser advance;
/// `Processor::sync_timeout` exposes only a shared reference, so the adapter
/// uses cells for the caller's clock and VTE's timeout state. The deadline is
/// the authority for both runtime expiry and VTE buffering, so a deadline
/// that cannot be represented never leaves output buffered without an expiry.
#[derive(Debug, Default)]
struct SyncUpdateTimeout {
    now: ClockCell<Option<Instant>>,
    deadline: ClockCell<Option<Instant>>,
}

impl SyncUpdateTimeout {
    fn set_now(&self, now: Instant) {
        self.now.set(Some(now));
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline.get()
    }
}

/// The one rule for selecting the active alacritty grid.
fn primary_screen_active<T>(term: &Term<T>) -> bool {
    !term.mode().contains(TermMode::ALT_SCREEN)
}

impl Timeout for SyncUpdateTimeout {
    fn set_timeout(&mut self, duration: std::time::Duration) {
        // Every parser advance sets `now` first, so the clock read is only a
        // guard: a buffering frame always gets a deadline.
        // clock-io-ok: unreachable while `advance` sets the caller's clock.
        let now = self.now.get().unwrap_or_else(Instant::now);
        // A duration past the clock's range expires the frame at once rather
        // than leaving output buffered with no deadline.
        self.deadline
            .set(Some(now.checked_add(duration).unwrap_or(now)));
    }

    fn clear_timeout(&mut self) {
        self.deadline.set(None);
    }

    fn pending_timeout(&self) -> bool {
        self.deadline.get().is_some()
    }
}

impl Terminal {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Self {
        let grid = shepr_core::geometry::GridSize::clamped_pane(cols, rows);
        let columns = usize::from(grid.cols.get());
        let screen_lines = usize::from(grid.rows.get());
        let history_lines = scrollback_lines(max_scrollback, columns);
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut term = Term::new(
            term_config(history_lines),
            &TermSize {
                columns,
                screen_lines,
            },
            Listener(Arc::clone(&events)),
        );
        // alacritty starts fully damaged; our own generation counters already
        // start "full", so begin alacritty's tracking from a clean slate.
        term.reset_damage();
        Self {
            term,
            parser: Processor::new(),
            keyboard_depth: KeyboardStackDepth::default(),
            events,
            scanner: Scanner::default(),
            max_scrollback,
            history_lines,
            default_palette: default_palette(),
            cell: None,
            modes: ExtraModes::default(),
            color_scheme: None,
            host_foreground: None,
            host_background: None,
            responses: Vec::new(),
            pwd_changes: Vec::new(),
            clipboard_writes: Vec::new(),
            dropped_clipboard_store_bytes: Vec::new(),
            title_update: None,
            progress_update: None,
            default_color_set: false,
            damage_generation: 1,
            full_damage_generation: 1,
            row_damage_generations: vec![0; screen_lines],
            rows: RowOrigin::default(),
        }
    }

    /// Feed child output into the terminal. Replies are queued in byte order
    /// and collected with [`Terminal::take_effects`].
    ///
    /// Everything vte dispatches to a `Handler` (including the adapter's own
    /// modes, RIS, DECRQM and the voiced-mark printing in `handler.rs`) is
    /// applied in byte order, and inside a synchronized update (mode 2026) only
    /// when vte replays the buffered frame. Scanner events with a spelling vte
    /// dispatches are fed back through the parser at their byte offset too.
    /// Queries such as XTGETTCAP, `CSI ? 996 n` and `CSI 16 t` have no such
    /// dispatch point, so their replies are applied as their bytes arrive. This
    /// puts scanner replies ahead of core replies requested earlier in the same
    /// frame. Deferring replies to the end of the frame would put them after a
    /// DA1 sentinel's answer, so the program could conclude the capability is
    /// missing and read the late reply as input. Working-directory and progress
    /// reports are side-band observations of the child's live state; they do
    /// not change parser or emulator state and are collected as they arrive.
    pub fn write(&mut self, bytes: &[u8]) {
        // clock-io-ok: callers without a read boundary of their own write now.
        self.write_at(bytes, Instant::now());
    }

    /// Feed child output using the caller's clock for synchronized-update
    /// deadlines. The ordinary `write` entry point supplies the current time.
    pub fn write_at(&mut self, bytes: &[u8], now: Instant) {
        if bytes.is_empty() {
            return;
        }
        let mut skipping_oversized_osc = self.scanner.has_oversized_osc();
        let events = self.scanner.scan(bytes);
        let mut written = 0usize;
        // An input without scanner events takes one parser batch. Keep event
        // boundaries so injected spellings and scanner replies stay at their
        // original byte positions, including inside synchronized updates.
        for scanned in events {
            let end = scanned.end.min(bytes.len());
            if end > written {
                if !skipping_oversized_osc {
                    self.advance(&bytes[written..end], now);
                }
                written = end;
            }
            match scanned.event {
                ScanEvent::AbortOversizedOsc => {
                    // vte's std parser retains OSC bodies without a size cap.
                    // End it at our bound, then omit the remainder until the
                    // scanner sees the original terminator.
                    self.advance(b"\x18", now);
                    skipping_oversized_osc = true;
                }
                ScanEvent::ResumeAfterOversizedOsc => skipping_oversized_osc = false,
                event => self.apply_scan_event(event, now),
            }
        }
        if written < bytes.len() && !skipping_oversized_osc {
            self.advance(&bytes[written..], now);
        }
    }

    fn advance(&mut self, bytes: &[u8], now: Instant) {
        self.parser.sync_timeout().set_now(now);
        self.with_handler(|handler, parser| parser.advance(handler, bytes));
    }

    /// Runs an operation with the parser and its handler inside one row batch.
    /// Every parser-driven terminal mutation must use this entry point so the
    /// row tracker and queued events are settled together.
    fn with_handler<R>(
        &mut self,
        operation: impl FnOnce(&mut CoreHandler<'_, Listener>, &mut Processor<SyncUpdateTimeout>) -> R,
    ) -> R {
        let Self {
            term,
            parser,
            keyboard_depth,
            modes,
            cell,
            events,
            default_color_set,
            rows,
            history_lines,
            max_scrollback,
            default_palette,
            host_foreground,
            host_background,
            ..
        } = self;

        rows.begin(term);
        let result = {
            let mut handler = CoreHandler {
                term,
                keyboard_depth,
                modes,
                cell: *cell,
                events,
                default_color_set,
                rows,
                history_limit: history_lines,
                max_scrollback: *max_scrollback,
                default_palette,
                host_foreground: *host_foreground,
                host_background: *host_background,
            };
            operation(&mut handler, parser)
        };
        rows.finish(term, *history_lines);
        self.drain_events();
        self.collect_damage();
        result
    }

    /// The xterm modifyOtherKeys level the child selected (0, 1 or 2).
    pub fn modify_other_keys_level(&self) -> ModifyOtherKeysLevel {
        self.modes.modify_other_keys
    }

    /// Ends a synchronized update (mode 2026) whose timeout has passed so its
    /// buffered output becomes visible. The runtime calls this from its tick
    /// path, before rendering or before parsing later child output. Returns
    /// whether an expired update was ended, even if it contained no visible
    /// changes.
    ///
    /// VTE's shepr-owned timeout stores the frame deadline; supplying `now`
    /// here keeps expiry checks runtime-driven and lets tests exercise either
    /// side of that deadline without sleeping.
    ///
    /// The frame's effects (replies, clipboard writes, title and colour
    /// changes) stay queued like any other write's, for whoever collects
    /// them next; nothing here discards them.
    pub fn tick(&mut self, now: Instant) -> bool {
        // vte arms this deadline on BSU; ending the frame also applies its mode
        // transition when the buffered content made no visible changes.
        let expired = self
            .parser
            .sync_timeout()
            .deadline()
            .is_some_and(|deadline| now >= deadline);
        if expired {
            self.parser.sync_timeout().set_now(now);
            self.with_handler(|handler, parser| parser.stop_sync(handler));
        }
        expired
    }

    /// When the pending synchronized update will be force-ended, if one is active.
    pub fn synchronized_output_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().deadline()
    }

    pub fn take_pty_responses(&mut self) -> Vec<PtyResponse> {
        mem::take(&mut self.responses)
    }

    /// Collect every queued effect at one boundary. The returned value can be
    /// dropped when the caller intentionally discards all effects.
    pub fn take_effects(&mut self) -> TerminalEffects {
        TerminalEffects {
            pty_responses: mem::take(&mut self.responses),
            pwd_changes: mem::take(&mut self.pwd_changes),
            clipboard_writes: mem::take(&mut self.clipboard_writes),
            dropped_clipboard_store_bytes: mem::take(&mut self.dropped_clipboard_store_bytes),
            title_update: self.title_update.take(),
            progress_update: self.progress_update.take(),
            default_color_set: mem::take(&mut self.default_color_set),
        }
    }

    fn push_bytes(&mut self, bytes: Vec<u8>) {
        self.responses.push(PtyResponse::Bytes(bytes));
    }

    fn apply_scan_event(&mut self, event: ScanEvent, now: Instant) {
        match event {
            // `write_at` consumes these boundaries while slicing parser input.
            ScanEvent::AbortOversizedOsc | ScanEvent::ResumeAfterOversizedOsc => {}
            ScanEvent::ColorSchemeQuery => {
                if let Some(scheme) = self.color_scheme {
                    self.push_bytes(scheme.report().to_vec());
                }
            }
            ScanEvent::CellSizeQuery => {
                if let Some(cell) = self.cell {
                    let reply = format!("\x1b[6;{};{}t", cell.height, cell.width);
                    self.push_bytes(reply.into_bytes());
                }
            }
            ScanEvent::Xtgettcap(replies) => {
                for reply in replies {
                    self.push_bytes(reply);
                }
            }
            // Working-directory and progress payloads describe live child
            // state, not parser state, so publish them as bytes arrive.
            ScanEvent::WorkingDirectory(payload) => self.pwd_changes.push(payload),
            ScanEvent::Progress(payload) => self.progress_update = Some(payload),
            // The parser has just consumed (and ignored) `CSI ? 3 J`; feed the
            // ED3 spelling it does dispatch. Going through the parser keeps
            // the erase in byte order even inside a synchronized update.
            ScanEvent::EraseScrollback => self.advance(b"\x1b[3J", now),
            // Feed a spelling vte dispatches rather than updating adapter
            // state here. During synchronized output the parser buffers these
            // bytes and replays them in order with the surrounding frame.
            ScanEvent::ModifyOtherKeys(level) => {
                let sequence = match level {
                    ModifyOtherKeysLevel::Off => b"\x1b[>4;0m".as_slice(),
                    ModifyOtherKeysLevel::ExceptWellDefined => b"\x1b[>4;1m".as_slice(),
                    ModifyOtherKeysLevel::All => b"\x1b[>4;2m".as_slice(),
                };
                self.advance(sequence, now);
            }
        }
    }

    fn push_in_band_size_report(&mut self) {
        if let Some(report) = handler::in_band_size_report(self.current_geometry()) {
            self.push_bytes(report.into_bytes());
        }
    }

    fn current_geometry(&self) -> shepr_core::geometry::PaneGeometry {
        handler::geometry_for_terminal(self.term.columns(), self.term.screen_lines(), self.cell)
    }

    fn drain_events(&mut self) {
        let events = {
            let mut queue = crate::lock_auxiliary(&self.events);
            mem::take(&mut *queue)
        };
        for event in events {
            match event {
                TerminalEvent::PtyWrite(bytes) => self.push_bytes(bytes),
                TerminalEvent::ColorQuery(query) => {
                    self.responses.push(PtyResponse::ColorQuery(query));
                }
                TerminalEvent::ClipboardStore(ClipboardType::Clipboard, text)
                    if !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES =>
                {
                    self.clipboard_writes.push(text.into_bytes());
                }
                TerminalEvent::ClipboardStore(ClipboardType::Clipboard, text)
                    if text.len() > MAX_CLIPBOARD_BYTES =>
                {
                    // `text` is already decoded valid UTF-8, so `len()` is
                    // the decoded OSC 52 store size in bytes. Keep only that
                    // count for the pane's diagnostic; never retain the text.
                    self.dropped_clipboard_store_bytes.push(text.len());
                }
                TerminalEvent::Title(title) => self.title_update = Some(TitleUpdate::Set(title)),
                TerminalEvent::ResetTitle => self.title_update = Some(TitleUpdate::Reset),
                _ => {}
            }
        }
    }

    /// Whether the child chose a cursor shape (DECSCUSR 1-6 or OSC 50) that
    /// is still in effect.
    pub fn cursor_shape_overridden(&self) -> bool {
        self.modes.cursor_shape_set
    }

    pub fn resize(&mut self, geometry: shepr_core::geometry::PaneGeometry) {
        let cols = geometry.cols();
        let rows = geometry.rows();
        let cell = geometry.cell();
        let columns = usize::from(cols);
        let screen_lines = usize::from(rows);
        let columns_changed = columns != self.term.columns();
        let lines_changed = screen_lines != self.term.screen_lines();
        let geometry_changed = columns_changed || lines_changed || cell != self.cell;

        // A column change re-wraps every line, so no earlier row id may keep
        // naming one. So does any reflow of the primary screen while the
        // alternate one is active: the tracker cannot see the inactive grid.
        // A height change on the primary screen only moves lines between
        // screen and history (evicting at the history limit), which the
        // tracker follows.
        let alternate = !primary_screen_active(&self.term);
        let rewraps = columns_changed || (alternate && lines_changed);
        if rewraps {
            self.rows.invalidate_primary(&self.term);
        } else {
            self.rows.begin(&self.term);
            // A shorter screen pushes its top lines into history.
            self.rows
                .count_pushed(self.term.screen_lines().saturating_sub(screen_lines));
        }

        // Grow capacity before reflow. A height change never lowers it:
        // growing the height pulls history onto the screen, and a capacity
        // floor at the shrunken history would leave no room to put those
        // lines back on the next shrink, recycling the oldest retained rows.
        let budget_lines = scrollback_lines(self.max_scrollback, columns);
        if budget_lines > self.history_lines {
            self.set_history_lines(budget_lines);
        }
        self.term.resize(TermSize {
            columns,
            screen_lines,
        });
        // The byte budget buys fewer lines at a wider width. After a width
        // change on the primary screen (row ids are retired by the rewrap
        // anyway), lower the capacity toward the new budget, but never below
        // the lines held in history and on screen: dropping content that fit
        // before would make a zoom/unzoom cycle destroy scrollback, and the
        // screen lines must still fit when a later shrink pushes them back.
        // A widened pane can thus hold more than its byte budget, bounded by
        // the content it already had, until it narrows or its history is
        // cleared. The alternate screen hides the primary history size, so
        // the capacity is left alone there.
        if columns_changed && !alternate {
            let held = self
                .term
                .history_size()
                .saturating_add(self.term.screen_lines());
            let settled = budget_lines.max(held).min(self.history_lines);
            if settled < self.history_lines {
                self.set_history_lines(settled);
            }
        }
        if rewraps {
            self.rows.observe(&self.term);
        } else {
            self.rows.finish(&self.term, self.history_lines);
        }
        self.cell = cell;
        self.drain_events();
        self.collect_damage();
        if geometry_changed && self.modes.in_band_resize {
            self.push_in_band_size_report();
        }
    }

    fn set_history_lines(&mut self, history_lines: usize) {
        HistoryCapacity::new(
            &mut self.term,
            &self.events,
            &mut self.history_lines,
            self.max_scrollback,
        )
        .set(history_lines);
    }

    pub fn set_color_scheme(&mut self, color_scheme: Option<ColorScheme>) -> Option<ColorScheme> {
        mem::replace(&mut self.color_scheme, color_scheme)
    }

    /// The live value of a DEC private mode; `false` when the table in
    /// `modes.rs` reports it as unsupported.
    pub fn mode_get(&self, mode: DecMode) -> bool {
        let spec = modes::lookup(mode);
        match spec.get {
            modes::Getter::Term(flag) if flag == TermMode::ALT_SCREEN => {
                !primary_screen_active(&self.term)
            }
            modes::Getter::Term(flag) => self.term.mode().contains(flag),
            modes::Getter::CursorBlink => self.term.cursor_style().blinking,
            modes::Getter::Extra(extra) => extra.get(&self.modes),
            modes::Getter::SynchronizedOutput => self.synchronized_output_deadline().is_some(),
            modes::Getter::Unsupported => false,
        }
    }

    /// Active kitty keyboard flags (bit 0 disambiguate through bit 4 associated text).
    pub fn kitty_keyboard_flags(&self) -> u16 {
        let mode = *self.term.mode();
        let mut flags = 0;
        for (term_mode, bit) in [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ] {
            if mode.contains(term_mode) {
                flags |= bit;
            }
        }
        flags
    }

    pub fn mouse_tracking_enabled(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE) || self.modes.x10_mouse
    }

    pub fn active_screen(&self) -> ActiveScreen {
        if primary_screen_active(&self.term) {
            ActiveScreen::Primary
        } else {
            ActiveScreen::Alternate
        }
    }

    pub fn total_rows(&self) -> usize {
        self.term.total_lines()
    }

    pub fn scrollbar(&self) -> TerminalScrollbar {
        let history = self.term.history_size();
        TerminalScrollbar {
            total: self.term.total_lines(),
            offset: history.saturating_sub(self.term.grid().display_offset()),
            len: self.term.screen_lines(),
        }
    }

    /// The absolute row id of screen row 0, the oldest retained line.
    ///
    /// A primary-screen line's absolute row id is `history_origin() + its
    /// screen row`. Primary screen rows shift under a caller whenever lines
    /// leave the top of the retained buffer (history at its line limit evicting
    /// its oldest line on every new one, `ED 3`, the host's clear); absolute
    /// ids do not: an id names the same primary line for as long as it is
    /// retained and is never reused, and ids below the origin name lines that
    /// are gone. A column change (which re-wraps every line) and RIS move the
    /// origin past every earlier id. On the alternate screen and on a primary
    /// screen without scrollback, nothing identifies a line once it scrolls
    /// off: rows there are viewport rows and the origin stays put (`rows.rs`).
    /// Alternate viewport coordinates can therefore numerically collide with
    /// primary row ids and identify only the current viewport row, not a stable
    /// line. Consumers retaining an alternate-screen location must include
    /// the active screen and invalidate the location when the screen changes.
    pub fn history_origin(&self) -> AbsRow {
        AbsRow(self.rows.origin())
    }

    /// The screen row of an absolute row id, `None` for a line that is no
    /// longer (or not yet) retained.
    pub fn screen_row_for_absolute(&self, row: AbsRow) -> Option<ScreenRow> {
        let y = usize::try_from(row.0.checked_sub(self.rows.origin())?).ok()?;
        (y < self.term.total_lines()).then_some(ScreenRow(y))
    }

    /// The absolute row id of screen row `y`.
    pub fn absolute_row_for_screen(&self, y: ScreenRow) -> AbsRow {
        self.rows
            .origin()
            .saturating_add(u64::try_from(y.0).unwrap_or(u64::MAX))
            .into()
    }

    /// Clears the screen and scrollback but keeps the cursor's (possibly
    /// soft-wrapped) line, moved to the top of the screen together with any
    /// DECSC-saved cursor position. Cleared rows are blank in default colours,
    /// whatever SGR the child has active. A no-op returning
    /// `AlternateScreenActive` while the alternate screen is active: the full-screen app owns
    /// that screen, and the primary history must survive until it exits.
    pub fn clear_screen(&mut self) -> ClearScreenOutcome {
        if !primary_screen_active(&self.term) {
            return ClearScreenOutcome::AlternateScreenActive;
        }
        let screen_lines = self.term.screen_lines();
        let last_column = self.term.last_column();
        let history = self.term.history_size();
        let grid = self.term.grid_mut();
        // This is a host action, not the child's erase, so vacated rows are
        // blank in default colours rather than filled with the child's current
        // pen (`scroll_up` and `reset_region` fill from the cursor template).
        let pen = mem::take(&mut grid.cursor.template);
        let mut top = grid.cursor.point.line;
        while top > Line(0)
            && grid[Line(top.0 - 1)][last_column]
                .flags
                .contains(Flags::WRAPLINE)
        {
            top = Line(top.0 - 1);
        }
        let shift = usize::try_from(top.0).unwrap_or(0);
        let screen_lines_line = i32::try_from(screen_lines).unwrap_or(i32::MAX);
        if shift > 0 {
            grid.scroll_up(&(Line(0)..Line(screen_lines_line)), shift);
            // `shift` was derived from `top.0` (an i32) above, so this round-trips losslessly.
            let shift_i32 = i32::try_from(shift).unwrap_or(i32::MAX);
            grid.cursor.point.line = Line(grid.cursor.point.line.0 - shift_i32);
            // Keep a DECSC-saved position on the same content; one whose row
            // was cleared away is pinned to the top.
            let saved = &mut grid.saved_cursor.point.line;
            *saved = Line((saved.0 - shift_i32).max(0));
        }
        // Keep the rest of the logical line too: the cursor may sit on an
        // earlier row of soft-wrapped input.
        let last_line = Line(screen_lines_line - 1);
        let mut bottom = grid.cursor.point.line;
        while bottom < last_line && grid[bottom][last_column].flags.contains(Flags::WRAPLINE) {
            bottom = Line(bottom.0 + 1);
        }
        let kept_rows = usize::try_from(bottom.0 + 1).unwrap_or(1);
        if kept_rows < screen_lines {
            grid.reset_region(Line(i32::try_from(kept_rows).unwrap_or(i32::MAX))..);
        }
        grid.cursor.template = pen;
        grid.clear_history();
        // Everything above the kept line is gone: the history and the
        // `shift` screen rows the kept line moved up over.
        self.rows.evict(history.saturating_add(shift));
        self.restore_scrollback_budget_after_history_purge();
        self.bump_full_damage();
        ClearScreenOutcome::Cleared
    }

    fn restore_scrollback_budget_after_history_purge(&mut self) {
        HistoryCapacity::new(
            &mut self.term,
            &self.events,
            &mut self.history_lines,
            self.max_scrollback,
        )
        .restore_after_history_purge();
    }

    pub fn scroll_viewport_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
        self.collect_damage();
    }

    /// Scrolls the viewport by `delta` rows; negative values move toward
    /// older history.
    pub fn scroll_viewport_delta(&mut self, delta: isize) {
        let delta = i32::try_from(
            delta
                .saturating_neg()
                .clamp(i32::MIN as isize, i32::MAX as isize),
        )
        .unwrap_or(i32::MAX);
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
            self.collect_damage();
        }
    }

    /// Scrolls so the viewport's top row is screen row `row` (0 = oldest),
    /// clamped to the available history.
    pub fn scroll_viewport_row(&mut self, row: ScreenRow) {
        let history = self.term.history_size();
        let target_offset = history - row.0.min(history);
        let current = self.term.grid().display_offset();
        let target_offset_i64 = i64::try_from(target_offset).unwrap_or(i64::MAX);
        let current_i64 = i64::try_from(current).unwrap_or(i64::MAX);
        // row.min(history) keeps the first subtraction nonnegative. These
        // offsets are in [0, i64::MAX], so their difference cannot overflow
        // i64; clamp to Scroll::Delta's range before converting to i32.
        let delta = i32::try_from(
            (target_offset_i64 - current_i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)),
        )
        .unwrap_or(i32::MAX);
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
            self.collect_damage();
        }
    }

    pub fn cols(&self) -> u16 {
        saturating_u16(self.term.columns())
    }

    pub fn rows(&self) -> u16 {
        saturating_u16(self.term.screen_lines())
    }

    pub fn cursor_y(&self) -> u16 {
        let line = self.term.grid().cursor.point.line.0.max(0);
        u16::try_from(line).unwrap_or(u16::MAX)
    }

    pub fn width_px(&self) -> u32 {
        self.current_geometry()
            .text_area_px()
            .map_or(0, |(width, _)| u32::from(width))
    }

    pub fn height_px(&self) -> u32 {
        self.current_geometry()
            .text_area_px()
            .map_or(0, |(_, height)| u32::from(height))
    }
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
use cell::cell_style;

#[cfg(test)]
impl PtyResponse {
    /// The reply the terminal would send on its own (colour queries answered
    /// with `core_color`, dropped when that is unset).
    fn into_core_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::ColorQuery(query) => query.core_color.map(|color| query.encode(color)),
        }
    }
}

#[cfg(test)]
impl Terminal {
    pub fn scrollback_rows(&self) -> usize {
        self.term.history_size()
    }

    /// The cursor colour set with OSC 12, if any.
    fn effective_cursor_color(&self) -> Option<RgbColor> {
        self.term.colors()[NamedColor::Cursor].map(RgbColor::from_vte)
    }
}

#[cfg(test)]
mod tests;
