//! Shepr's terminal-core boundary.
//!
//! The emulator underneath is `alacritty_terminal` (pinned in Cargo.toml). The
//! rest of the tree only sees the types in this module; alacritty types stay
//! private here. The adapter is contained in `src/vt/`.
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
mod locks;
mod modes;
mod read;
mod render;
mod rows;
mod scan;
pub use cell::RenderColors;
#[cfg(test)]
use cell::cell_style;
#[cfg(test)]
pub use cell::test_unicode_grapheme_width;
pub use cell::{
    CellBasicData, CellColor, CellView, CellWide, UnderlineStyle, unicode_codepoint_width,
    unicode_text_width,
};
use cell::{CellText, cell_graphemes, cell_text, cell_text_into, cell_wide};
pub(crate) use cell::{RowWrap, ScreenTextCell, ScreenTextRow, unicode_display_units};
pub const PANE_TERM: &str = "xterm-256color";

pub use color::{ColorQuery, ColorQueryTarget, DefaultColor, RgbColor, default_palette};
pub use render::{CursorVisualStyle, Dirty, RenderState};
pub(crate) use scan::{ProgressReport, WorkingDirectoryReport};

pub(crate) use locks::{
    TerminalCorePoisoned, lock_auxiliary, lock_terminal_core, recover_auxiliary_poison,
    terminal_core_is_poisoned,
};
#[cfg(test)]
pub(crate) use locks::{TerminalCoreTryLockError, try_lock_auxiliary, try_lock_terminal_core};

use std::fmt;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, Handler, NamedColor, Processor, Rgb};
use unicode_width::UnicodeWidthChar;

pub(crate) use coords::Point;
pub use coords::{AbsRow, ScreenRow, ViewportRow};

use self::format::Format;
use self::handler::{CoreHandler, KeyboardStackDepth};
use self::rows::RowOrigin;
use self::scan::{ScanEvent, Scanner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(&'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "terminal error: {}", self.0)
    }
}

impl std::error::Error for Error {}

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

pub const MODE_APPLICATION_CURSOR_KEYS: u16 = 1;
pub const MODE_CURSOR_BLINK: u16 = 12;
pub const MODE_FOCUS_EVENT: u16 = 1004;
pub const MODE_MOUSE_UTF8: u16 = 1005;
pub const MODE_MOUSE_SGR: u16 = 1006;
pub const MODE_MOUSE_ALTERNATE_SCROLL: u16 = 1007;
pub const MODE_MOUSE_SGR_PIXELS: u16 = 1016;
pub const MODE_URGENCY_HINTS: u16 = 1042;
pub const MODE_BRACKETED_PASTE: u16 = 2004;
pub const MODE_SYNCHRONIZED_OUTPUT: u16 = 2026;
pub const MODE_COLOR_SCHEME_REPORT: u16 = 2031;

// Unicode private-use codepoint used by the kitty graphics unicode-placeholder
// convention. Shepr does not render kitty graphics, but programs may still
// emit this codepoint as literal text; keep filtering it out of copied,
// history and rendered text so stray placeholder glyphs don't leak into
// user-visible output.
pub(crate) const KITTY_UNICODE_PLACEHOLDER: u32 = 0x10EEEE;

/// Default colours reported while the program and host have set none. They
/// match what the libghostty-vt render state reported, which the pane layer
/// compares against to decide whether a default colour is "unchanged".
const DEFAULT_FOREGROUND: RgbColor = RgbColor {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};
const DEFAULT_BACKGROUND: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

/// alacritty needs two columns to hold a wide character without panicking.
const MIN_COLUMNS: usize = 2;

/// Scrollback is configured in bytes; alacritty counts lines. Any non-zero
/// byte budget keeps at least this many lines so tiny budgets still scroll,
/// which means a small budget on a wide pane is exceeded by design.
const MIN_SCROLLBACK_LINES: usize = 1_000;
/// Sanity cap on the converted line count. The byte budget alone does not
/// bound memory: the line floor above, heap-held cell extras (combining
/// marks, hyperlinks) and history kept across a widening resize (see
/// [`Terminal::resize`]) all go past it.
const MAX_SCROLLBACK_LINES: usize = 1_000_000;

const MAX_CLIPBOARD_BYTES: usize = 192 * 1024;

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

#[cfg(test)]
impl PtyResponse {
    /// The reply the terminal would send on its own (colour queries answered
    /// with `core_color`, dropped when that is unset).
    pub fn into_core_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::ColorQuery(query) => query.core_color.map(|color| query.encode(color)),
        }
    }
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
    let bytes_per_line = columns.max(1).saturating_mul(mem::size_of::<Cell>()).max(1);
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

/// Collects the alacritty events the adapter acts on, in emission order.
/// Bells are not among them: nothing in shepr surfaces a bell.
#[derive(Clone)]
struct Listener(Arc<Mutex<Vec<Event>>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let relevant = matches!(
            event,
            Event::PtyWrite(_)
                | Event::ColorRequest(..)
                | Event::ClipboardStore(..)
                | Event::Title(_)
                | Event::ResetTitle
        );
        if relevant {
            crate::vt::lock_auxiliary(&self.0).push(event);
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

/// Result of a host-requested clear operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearScreenOutcome {
    Cleared,
    AlternateScreenActive,
}

pub struct Terminal {
    term: Term<Listener>,
    parser: Processor,
    /// Mirror of alacritty's keyboard-mode stack depths; the parser must only
    /// ever drive `term` through a [`CoreHandler`] so it stays exact.
    keyboard_depth: KeyboardStackDepth,
    events: Arc<Mutex<Vec<Event>>>,
    scanner: Scanner,
    max_scrollback: usize,
    history_lines: usize,
    default_palette: [RgbColor; 256],
    cell: Option<crate::geometry::CellPx>,
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
    /// The latest title change not yet collected.
    title_update: Option<TitleUpdate>,
    /// The latest OSC 9;4 progress payload (after `9;`) not yet collected.
    progress_update: Option<ProgressReport>,
    /// The child set the default foreground or background since the last
    /// [`Terminal::take_default_color_set`].
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

impl Terminal {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Self {
        let columns = usize::from(cols).max(MIN_COLUMNS);
        let screen_lines = usize::from(rows).max(1);
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
    /// and collected with [`Terminal::take_pty_responses`].
    ///
    /// Everything vte dispatches to a `Handler` (including the adapter's own
    /// modes, RIS, DECRQM and the voiced-mark printing in `handler.rs`) is
    /// applied in byte order, and inside a synchronized update (mode 2026) only
    /// when vte replays the buffered frame. The scanner's events cannot be: vte
    /// never hands OSC 7, XTGETTCAP, `CSI ? 996 n` or `CSI 16 t` to a handler,
    /// and it replays a frame in one call, so there is no point at which to
    /// slot them in. They are applied as their bytes arrive, which inside a
    /// frame puts their replies ahead of core replies requested earlier in the
    /// same frame. Deferring them to the end of the frame instead was
    /// rejected: queries are almost always followed by a DA1 sentinel, and a
    /// deferred reply would land after the sentinel's answer, so the program
    /// would conclude the capability is missing and read the late reply as
    /// input.
    pub fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.flush_expired_synchronized_output();
        let events = self.scanner.scan(bytes);
        let mut written = 0usize;
        for scanned in events {
            let end = scanned.end.min(bytes.len());
            if end > written {
                self.advance(&bytes[written..end]);
                written = end;
            }
            self.apply_scan_event(scanned.event);
        }
        if written < bytes.len() {
            self.advance(&bytes[written..]);
        }
        self.collect_damage();
    }

    fn advance(&mut self, bytes: &[u8]) {
        self.with_handler(|handler, parser| parser.advance(handler, bytes));
    }

    /// Runs an operation with the parser and its handler inside one row batch.
    /// Every parser-driven terminal mutation must use this entry point so the
    /// row tracker and queued events are settled together.
    fn with_handler<R>(
        &mut self,
        operation: impl FnOnce(&mut CoreHandler<'_, Listener>, &mut Processor) -> R,
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
                history_limit: *history_lines,
            };
            operation(&mut handler, parser)
        };
        rows.finish(term, *history_lines);
        self.drain_events();
        result
    }

    /// The xterm modifyOtherKeys level the child selected (0, 1 or 2).
    pub fn modify_other_keys_level(&self) -> ModifyOtherKeysLevel {
        self.modes.modify_other_keys
    }

    /// Ends a synchronized update (mode 2026) whose timeout has passed so its
    /// buffered output becomes visible. Returns whether anything was flushed.
    ///
    /// The frame's effects (replies, clipboard writes, title and colour
    /// changes) stay queued like any other write's, for whoever collects
    /// them next; nothing here discards them.
    pub fn flush_expired_synchronized_output(&mut self) -> bool {
        let expired = self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| Instant::now() >= deadline);
        if expired {
            self.with_handler(|handler, parser| parser.stop_sync(handler));
            self.collect_damage();
        }
        expired
    }

    /// When the pending synchronized update will be force-ended, if one is active.
    pub fn synchronized_output_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    pub fn take_pty_responses(&mut self) -> Vec<PtyResponse> {
        mem::take(&mut self.responses)
    }

    /// Puts replies taken with [`Terminal::take_pty_responses`] back at the
    /// front of the queue, ahead of anything queued since, for a caller that
    /// only wanted the replies of one operation.
    pub fn restore_pty_responses(&mut self, mut responses: Vec<PtyResponse>) {
        if responses.is_empty() {
            return;
        }
        responses.append(&mut self.responses);
        self.responses = responses;
    }

    fn push_bytes(&mut self, bytes: Vec<u8>) {
        self.responses.push(PtyResponse::Bytes(bytes));
    }

    fn apply_scan_event(&mut self, event: ScanEvent) {
        match event {
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
            ScanEvent::WorkingDirectory(payload) => self.pwd_changes.push(payload),
            ScanEvent::Progress(payload) => self.progress_update = Some(payload),
            // The parser has just consumed (and ignored) `CSI ? 3 J`; feed the
            // ED3 spelling it does dispatch. Going through the parser keeps
            // the erase in byte order even inside a synchronized update.
            ScanEvent::EraseScrollback => self.advance(b"\x1b[3J"),
            ScanEvent::ModifyOtherKeys(level) => self.modes.modify_other_keys = level,
        }
    }

    fn push_in_band_size_report(&mut self) {
        if let Some(report) =
            handler::in_band_size_report(self.term.screen_lines(), self.term.columns(), self.cell)
        {
            self.push_bytes(report.into_bytes());
        }
    }

    fn drain_events(&mut self) {
        let events = {
            let mut queue = crate::vt::lock_auxiliary(&self.events);
            mem::take(&mut *queue)
        };
        for event in events {
            match event {
                Event::PtyWrite(text) => self.push_bytes(text.into_bytes()),
                Event::ColorRequest(index, format) => {
                    if let Some(target) = ColorQueryTarget::from_index(index) {
                        let core_color = self.core_query_color(target);
                        let child_override = match target {
                            ColorQueryTarget::Foreground => self
                                .default_color_override(DefaultColor::Foreground)
                                .is_some(),
                            ColorQueryTarget::Background => self
                                .default_color_override(DefaultColor::Background)
                                .is_some(),
                            ColorQueryTarget::Palette(_) | ColorQueryTarget::Cursor => false,
                        };
                        self.responses.push(PtyResponse::ColorQuery(ColorQuery {
                            target,
                            core_color,
                            child_override,
                            format,
                        }));
                    }
                }
                Event::ClipboardStore(ClipboardType::Clipboard, text)
                    if !text.is_empty() && text.len() <= MAX_CLIPBOARD_BYTES =>
                {
                    self.clipboard_writes.push(text.into_bytes());
                }
                Event::Title(title) => self.title_update = Some(TitleUpdate::Set(title)),
                Event::ResetTitle => self.title_update = Some(TitleUpdate::Reset),
                _ => {}
            }
        }
    }

    /// Whether the child set a default foreground or background since the
    /// last call.
    pub fn take_default_color_set(&mut self) -> bool {
        mem::take(&mut self.default_color_set)
    }

    /// The latest window-title change (OSC 0/2, CSI 23 t, RIS) since the last
    /// call: `TitleUpdate::Reset` is a reset, `None` means no change. The title is
    /// exactly what vte parsed (trimmed, not otherwise sanitised).
    pub fn take_title_update(&mut self) -> Option<TitleUpdate> {
        self.title_update.take()
    }

    /// The latest OSC 9;4 progress payload (the text after `9;`) since the
    /// last call.
    pub fn take_progress_update(&mut self) -> Option<ProgressReport> {
        self.progress_update.take()
    }

    /// Whether the child chose a cursor shape (DECSCUSR 1-6 or OSC 50) that
    /// is still in effect.
    pub fn cursor_shape_overridden(&self) -> bool {
        self.modes.cursor_shape_set
    }

    pub fn resize(&mut self, geometry: crate::geometry::PaneGeometry) {
        let cols = geometry.cols();
        let rows = geometry.rows();
        let cell = geometry.cell;
        let columns = usize::from(cols).max(MIN_COLUMNS);
        let screen_lines = usize::from(rows).max(1);
        let columns_changed = columns != self.term.columns();
        let lines_changed = screen_lines != self.term.screen_lines();
        let geometry_changed = columns_changed || lines_changed || cell != self.cell;

        // A column change re-wraps every line, so no earlier row id may keep
        // naming one. So does any reflow of the primary screen while the
        // alternate one is active: the tracker cannot see the inactive grid.
        // A height change on the primary screen only moves lines between
        // screen and history (evicting at the history limit), which the
        // tracker follows.
        let alternate = self.term.mode().contains(TermMode::ALT_SCREEN);
        let rewraps = columns_changed || (alternate && lines_changed);
        if rewraps {
            self.rows.invalidate_primary(&self.term);
        } else {
            self.rows.begin(&self.term);
        }

        // The byte budget buys fewer lines at a wider width. Grow the line
        // limit before reflowing into more lines; afterwards lower it at most
        // to the history already held, never below it: dropping history that
        // fit before the resize would make a zoom/unzoom cycle, or attaching
        // from a wider client, destroy scrollback for good. The cost is that
        // a widened pane holds more than its byte budget until it narrows
        // again (or its history is cleared).
        let budget_lines = scrollback_lines(self.max_scrollback, columns);
        if budget_lines > self.history_lines {
            self.set_history_lines(budget_lines);
        }
        self.term.resize(TermSize {
            columns,
            screen_lines,
        });
        let history_lines = budget_lines.max(self.term.history_size().min(self.history_lines));
        if history_lines != self.history_lines {
            self.set_history_lines(history_lines);
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
        self.history_lines = history_lines;
        let queued = crate::vt::lock_auxiliary(&self.events).len();
        self.term.set_options(term_config(history_lines));
        // Term::set_options sends only the current title (or reset) through
        // this listener, synchronously. Term mutation is private to this
        // &mut Terminal API and no other event producer can reach this queue,
        // so truncating the tail drops only that synthetic reannouncement,
        // which is not a title change made by the child.
        crate::vt::lock_auxiliary(&self.events).truncate(queued);
    }

    pub fn set_color_scheme(&mut self, color_scheme: Option<ColorScheme>) -> Option<ColorScheme> {
        mem::replace(&mut self.color_scheme, color_scheme)
    }

    pub fn take_pwd_changes(&mut self) -> Vec<WorkingDirectoryReport> {
        mem::take(&mut self.pwd_changes)
    }

    pub fn take_clipboard_writes(&mut self) -> Vec<Vec<u8>> {
        mem::take(&mut self.clipboard_writes)
    }

    /// The live value of a DEC private mode; `false` for modes the table in
    /// `modes.rs` does not list or reports as unsupported.
    pub fn mode_get(&self, mode: u16) -> bool {
        let Some(spec) = modes::lookup(mode) else {
            return false;
        };
        match spec.get {
            modes::Getter::Term(flag) => self.term.mode().contains(flag),
            modes::Getter::CursorBlink => self.term.cursor_style().blinking,
            modes::Getter::Extra(extra) => extra.get(&self.modes),
            modes::Getter::SynchronizedOutput => self.synchronized_output_deadline().is_some(),
            modes::Getter::Unsupported => false,
        }
    }

    /// Sets a DEC private mode with the same effect as the child's
    /// `CSI ? mode h/l`, but through the handler directly: nothing is fed to
    /// the parser, so a sequence the child has half-written is not disturbed
    /// and a synchronized update does not defer it. Mode 2026 is refused: it
    /// is parser state, not terminal state.
    #[cfg(test)]
    pub fn mode_set(&mut self, mode: u16, value: bool) -> Result<(), Error> {
        if mode == MODE_SYNCHRONIZED_OUTPUT {
            return Err(Error("synchronized output is driven by the parser"));
        }
        let private_mode = handler::private_mode(mode);
        self.with_handler(|handler, _parser| {
            if value {
                Handler::set_private_mode(handler, private_mode);
            } else {
                Handler::unset_private_mode(handler, private_mode);
            }
        });
        self.collect_damage();
        Ok(())
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
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            ActiveScreen::Alternate
        } else {
            ActiveScreen::Primary
        }
    }

    pub fn total_rows(&self) -> usize {
        self.term.total_lines()
    }

    #[cfg(test)]
    pub fn scrollback_rows(&self) -> usize {
        self.term.history_size()
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
    /// A line's absolute row id is `history_origin() + its screen row`.
    /// Screen rows shift under a caller whenever lines leave the top of the
    /// retained buffer (history at its line limit evicting its oldest line on
    /// every new one, `ED 3`, the host's clear); absolute ids do not: an id
    /// names the same line for as long as it is retained and is never reused,
    /// and ids below the origin name lines that are gone. A column change
    /// (which re-wraps every line) and RIS move the origin past every earlier
    /// id. On the alternate screen, and on a primary screen without
    /// scrollback, nothing identifies a line once it scrolls off: rows there
    /// are viewport rows and the origin stays put (`rows.rs`).
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
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
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
        self.bump_full_damage();
        ClearScreenOutcome::Cleared
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

    /// The cursor colour set with OSC 12, if any.
    #[cfg(test)]
    pub fn effective_cursor_color(&self) -> Option<RgbColor> {
        self.term.colors()[NamedColor::Cursor].map(RgbColor::from)
    }

    pub(crate) fn width_px(&self) -> u32 {
        u32::try_from(self.term.columns())
            .unwrap_or(u32::MAX)
            .saturating_mul(self.cell.map_or(0, |cell| cell.width.get()))
    }

    pub(crate) fn height_px(&self) -> u32 {
        u32::try_from(self.term.screen_lines())
            .unwrap_or(u32::MAX)
            .saturating_mul(self.cell.map_or(0, |cell| cell.height.get()))
    }
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests;
