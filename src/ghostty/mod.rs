//! Shepr's terminal-core boundary.
//!
//! The emulator underneath is `alacritty_terminal` (pinned in Cargo.toml). The
//! rest of the tree only sees the types in this module; alacritty types stay
//! private here. The module keeps the name `ghostty` from the libghostty-vt
//! era until the planned rename to `vt`.
//!
//! What this adapter adds on top of alacritty:
//! * render snapshots with row dirty flags derived from alacritty's damage
//!   ([`RenderState`], [`RowIter`], [`RowCellIter`]);
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

// The adapter keeps a complete surface (mode constants, colour/scheme types,
// query helpers) even where the current tree uses only part of it.
#![allow(dead_code)]

mod format;
mod handler;
mod rows;
mod scan;

use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, Handler, NamedColor, Processor, Rgb};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
pub enum Dirty {
    Clean,
    Partial,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowSelection {
    pub start_x: u16,
    pub end_x: u16,
}

impl RowSelection {
    pub fn range(self) -> RangeInclusive<u16> {
        self.start_x..=self.end_x
    }
}

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
    fn report(self) -> &'static [u8] {
        match self {
            Self::Dark => b"\x1b[?997;1n",
            Self::Light => b"\x1b[?997;2n",
        }
    }
}

pub const MODE_APPLICATION_CURSOR_KEYS: u16 = 1;
pub const MODE_FOCUS_EVENT: u16 = 1004;
pub const MODE_MOUSE_UTF8: u16 = 1005;
pub const MODE_MOUSE_SGR: u16 = 1006;
pub const MODE_MOUSE_ALTERNATE_SCROLL: u16 = 1007;
pub const MODE_MOUSE_SGR_PIXELS: u16 = 1016;
pub const MODE_BRACKETED_PASTE: u16 = 2004;
pub const MODE_SYNCHRONIZED_OUTPUT: u16 = 2026;
pub const MODE_COLOR_SCHEME_REPORT: u16 = 2031;
pub const MODE_IN_BAND_RESIZE: u16 = 2048;

// Unicode private-use codepoint used by the kitty graphics unicode-placeholder
// convention. Shepr does not render kitty graphics, but programs may still
// emit this codepoint as literal text; keep filtering it out of copied text
// and history so stray placeholder glyphs don't leak into user-visible output.
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
pub enum CursorVisualStyle {
    Bar,
    Block,
    Underline,
    BlockHollow,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorViewport {
    pub x: u16,
    pub y: u16,
    pub wide_tail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCursor {
    pub viewport: Option<CursorViewport>,
    pub visible: bool,
    pub blinking: bool,
    pub visual_style: CursorVisualStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<Rgb> for RgbColor {
    fn from(value: Rgb) -> Self {
        Self {
            r: value.r,
            g: value.g,
            b: value.b,
        }
    }
}

impl From<RgbColor> for Rgb {
    fn from(value: RgbColor) -> Self {
        Rgb {
            r: value.r,
            g: value.g,
            b: value.b,
        }
    }
}

/// The built-in 256-colour palette used until the host theme overrides it.
pub fn default_palette() -> [RgbColor; 256] {
    const NAMED: [(u8, u8, u8); 16] = [
        (0x1d, 0x1f, 0x21),
        (0xcc, 0x66, 0x66),
        (0xb5, 0xbd, 0x68),
        (0xf0, 0xc6, 0x74),
        (0x81, 0xa2, 0xbe),
        (0xb2, 0x94, 0xbb),
        (0x8a, 0xbe, 0xb7),
        (0xc5, 0xc8, 0xc6),
        (0x66, 0x66, 0x66),
        (0xd5, 0x4e, 0x53),
        (0xb9, 0xca, 0x4a),
        (0xe7, 0xc5, 0x47),
        (0x7a, 0xa6, 0xda),
        (0xc3, 0x97, 0xd8),
        (0x70, 0xc0, 0xb1),
        (0xea, 0xea, 0xea),
    ];
    let mut palette = [RgbColor::default(); 256];
    for (slot, (r, g, b)) in palette.iter_mut().zip(NAMED) {
        *slot = RgbColor { r, g, b };
    }
    let cube = |value: usize| -> u8 {
        if value == 0 {
            0
        } else {
            // `value` is a 0..=5 cube coordinate, so `value * 40 + 55` maxes at 255.
            u8::try_from(value * 40 + 55).unwrap_or(u8::MAX)
        }
    };
    for (offset, slot) in palette[16..232].iter_mut().enumerate() {
        *slot = RgbColor {
            r: cube(offset / 36),
            g: cube((offset / 6) % 6),
            b: cube(offset % 6),
        };
    }
    for (offset, slot) in palette[232..256].iter_mut().enumerate() {
        // `offset` is 0..24 here, so `offset * 10 + 8` maxes at 238.
        let value = u8::try_from(offset * 10 + 8).unwrap_or(u8::MAX);
        *slot = RgbColor {
            r: value,
            g: value,
            b: value,
        };
    }
    palette
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    Palette(u8),
    Rgb(RgbColor),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg_color: Option<CellColor>,
    pub bg_color: Option<CellColor>,
    pub underline_color: Option<CellColor>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    /// Always false: alacritty does not model blink.
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    /// Always false: alacritty does not model overline.
    pub overline: bool,
    /// 0 none, 1 single, 2 double, 3 curly, 4 dotted, 5 dashed.
    pub underline: u8,
    pub underlined: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderColors {
    pub background: RgbColor,
    pub foreground: RgbColor,
    pub palette: [RgbColor; 256],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellWide {
    Narrow,
    Wide,
    SpacerTail,
    SpacerHead,
}

/// An owned copy of one cell's text. Building these costs an allocation per
/// non-blank cell (blank cells hold an empty, unallocated `Vec`); readers
/// that run per tick or over the whole history use
/// [`Terminal::visit_screen_row_text`] instead. The one remaining builder of
/// whole screens, the alternate-screen history read, copies a single
/// viewport per poll step of an explicit API read, so the per-cell `Vec` is
/// kept rather than moving every consumer to a packed representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScreenTextCell {
    pub wide: CellWide,
    pub graphemes: Vec<u32>,
}

/// How a row joins its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RowWrap {
    /// The row's text continues on the next row.
    pub soft_wrapped: bool,
    /// The row continues the previous row's text.
    pub wrap_continuation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScreenTextRow {
    pub cells: Vec<ScreenTextCell>,
    pub soft_wrapped: bool,
    pub wrap_continuation: bool,
}

/// Target of an OSC 4/10/11/12 colour query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorQueryTarget {
    Palette(u8),
    Foreground,
    Background,
    Cursor,
}

impl ColorQueryTarget {
    fn from_index(index: usize) -> Option<Self> {
        match index {
            0..=255 => Some(Self::Palette(u8::try_from(index).unwrap_or(u8::MAX))),
            index if index == NamedColor::Foreground as usize => Some(Self::Foreground),
            index if index == NamedColor::Background as usize => Some(Self::Background),
            index if index == NamedColor::Cursor as usize => Some(Self::Cursor),
            _ => None,
        }
    }
}

/// The default colours a child can override with OSC 10/11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultColor {
    Foreground,
    Background,
}

impl DefaultColor {
    fn named(self) -> NamedColor {
        match self {
            Self::Foreground => NamedColor::Foreground,
            Self::Background => NamedColor::Background,
        }
    }
}

/// An OSC colour query the child sent. The pane decides how to answer it;
/// `core_color` is what the terminal itself would report (child override,
/// then host default, then built-in palette; `None` for a foreground or
/// background nobody has set), captured at the query's position in the
/// stream.
pub struct ColorQuery {
    target: ColorQueryTarget,
    core_color: Option<RgbColor>,
    child_override: bool,
    format: Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>,
}

impl ColorQuery {
    pub fn target(&self) -> ColorQueryTarget {
        self.target
    }

    pub fn core_color(&self) -> Option<RgbColor> {
        self.core_color
    }

    /// Whether a default-colour query was answered from the child's own OSC
    /// 10/11 override at the moment it was asked (always false for palette
    /// and cursor queries).
    pub fn child_override(&self) -> bool {
        self.child_override
    }

    /// Encode a reply in the form the query asked for (same OSC number and
    /// terminator).
    pub fn encode(&self, color: RgbColor) -> Vec<u8> {
        (*self.format)(color.into()).into_bytes()
    }
}

impl fmt::Debug for ColorQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColorQuery")
            .field("target", &self.target)
            .field("core_color", &self.core_color)
            .field("child_override", &self.child_override)
            .finish_non_exhaustive()
    }
}

/// A reply the terminal wants written back to the child, in byte order.
#[derive(Debug)]
pub enum PtyResponse {
    Bytes(Vec<u8>),
    ColorQuery(ColorQuery),
}

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

pub fn unicode_codepoint_width(codepoint: u32) -> u8 {
    match char::from_u32(codepoint) {
        Some(ch) => u8::try_from(ch.width().unwrap_or(0).min(2)).unwrap_or(2),
        None => 1,
    }
}

/// Width of the first grapheme cluster in `codepoints`, returned as
/// `(codepoints consumed, cell width)`.
pub fn unicode_grapheme_width(codepoints: &[u32]) -> (usize, u8) {
    let Some(&first) = codepoints.first() else {
        return (0, 0);
    };
    if char::from_u32(first).is_none() {
        return (1, 1);
    }
    let text: String = codepoints
        .iter()
        .map_while(|&codepoint| char::from_u32(codepoint))
        .collect();
    let Some(cluster) = text.graphemes(true).next() else {
        return (0, 0);
    };
    let consumed = cluster.chars().count();
    (consumed, u8::try_from(cluster.width().min(2)).unwrap_or(2))
}

pub fn encode_focus(event: FocusEvent) -> Result<Vec<u8>, Error> {
    Ok(match event {
        FocusEvent::Gained => b"\x1b[I".to_vec(),
        FocusEvent::Lost => b"\x1b[O".to_vec(),
    })
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
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(event);
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
    modify_other_keys: u8,
    /// The child chose a cursor shape (DECSCUSR 1-6 or OSC 50) and has not
    /// asked for the default back (DECSCUSR 0, RIS).
    cursor_shape_set: bool,
    /// Between vte's BSU and ESU (or timeout) as the handler sees them, which
    /// inside a buffered frame is replay order. Only DECRQM ?2026 reads it;
    /// [`Terminal::mode_get`] asks the parser.
    synchronized_update: bool,
}

#[derive(Clone, Copy)]
enum Coordinates {
    Screen,
    Viewport,
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
    cell_width_px: u32,
    cell_height_px: u32,
    modes: ExtraModes,
    color_scheme: Option<ColorScheme>,
    /// The host's default foreground/background. They sit under the child's
    /// OSC 10/11 overrides (alacritty's `colors` slots) and over the built-in
    /// defaults, so host theme changes never go through the child's parser.
    host_foreground: Option<RgbColor>,
    host_background: Option<RgbColor>,
    responses: Vec<PtyResponse>,
    pwd_changes: Vec<Vec<u8>>,
    clipboard_writes: Vec<Vec<u8>>,
    /// The latest title change not yet collected: `Some(None)` is a reset.
    title_update: Option<Option<String>>,
    /// The latest OSC 9;4 progress payload (after `9;`) not yet collected.
    progress_update: Option<Vec<u8>>,
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
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, Error> {
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
        Ok(Self {
            term,
            parser: Processor::new(),
            keyboard_depth: KeyboardStackDepth::default(),
            events,
            scanner: Scanner::default(),
            max_scrollback,
            history_lines,
            default_palette: default_palette(),
            cell_width_px: 0,
            cell_height_px: 0,
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
        })
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
        self.rows.begin(&self.term);
        let mut handler = CoreHandler {
            term: &mut self.term,
            keyboard_depth: &mut self.keyboard_depth,
            modes: &mut self.modes,
            cell_width_px: self.cell_width_px,
            cell_height_px: self.cell_height_px,
            events: &self.events,
            default_color_set: &mut self.default_color_set,
            rows: &mut self.rows,
            history_limit: self.history_lines,
        };
        self.parser.advance(&mut handler, bytes);
        self.rows.finish(&self.term, self.history_lines);
        self.drain_events();
    }

    /// The xterm modifyOtherKeys level the child selected (0, 1 or 2).
    pub fn modify_other_keys_level(&self) -> u8 {
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
            self.rows.begin(&self.term);
            let mut handler = CoreHandler {
                term: &mut self.term,
                keyboard_depth: &mut self.keyboard_depth,
                modes: &mut self.modes,
                cell_width_px: self.cell_width_px,
                cell_height_px: self.cell_height_px,
                events: &self.events,
                default_color_set: &mut self.default_color_set,
                rows: &mut self.rows,
                history_limit: self.history_lines,
            };
            self.parser.stop_sync(&mut handler);
            self.rows.finish(&self.term, self.history_lines);
            self.drain_events();
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

    fn has_pixel_geometry(&self) -> bool {
        self.cell_width_px > 0 && self.cell_height_px > 0
    }

    fn apply_scan_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::ColorSchemeQuery => {
                if let Some(scheme) = self.color_scheme {
                    self.push_bytes(scheme.report().to_vec());
                }
            }
            ScanEvent::CellSizeQuery => {
                if self.has_pixel_geometry() {
                    let reply = format!("\x1b[6;{};{}t", self.cell_height_px, self.cell_width_px);
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
        if let Some(report) = handler::in_band_size_report(
            self.term.screen_lines(),
            self.term.columns(),
            self.cell_width_px,
            self.cell_height_px,
        ) {
            self.push_bytes(report.into_bytes());
        }
    }

    fn drain_events(&mut self) {
        let events = {
            let mut queue = self.events.lock().unwrap_or_else(PoisonError::into_inner);
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
                Event::Title(title) => self.title_update = Some(Some(title)),
                Event::ResetTitle => self.title_update = Some(None),
                _ => {}
            }
        }
    }

    /// What the terminal reports for a colour query: the child's override
    /// first, then the host default, then (for the palette) the built-in
    /// table. `None` for a default colour nobody has set.
    fn core_query_color(&self, target: ColorQueryTarget) -> Option<RgbColor> {
        let colors = self.term.colors();
        match target {
            ColorQueryTarget::Palette(index) => Some(self.effective_palette_color(index)),
            ColorQueryTarget::Foreground => colors[NamedColor::Foreground]
                .map(RgbColor::from)
                .or(self.host_foreground),
            ColorQueryTarget::Background => colors[NamedColor::Background]
                .map(RgbColor::from)
                .or(self.host_background),
            ColorQueryTarget::Cursor => colors[NamedColor::Cursor]
                .or(colors[NamedColor::Foreground])
                .map(RgbColor::from)
                .or(self.host_foreground),
        }
    }

    fn effective_palette_color(&self, index: u8) -> RgbColor {
        let index = usize::from(index);
        self.term.colors()[index]
            .map(RgbColor::from)
            .unwrap_or(self.default_palette[index])
    }

    fn render_colors(&self) -> RenderColors {
        let colors = self.term.colors();
        let mut palette = self.default_palette;
        for (index, slot) in palette.iter_mut().enumerate() {
            if let Some(color) = colors[index] {
                *slot = color.into();
            }
        }
        RenderColors {
            background: colors[NamedColor::Background]
                .map(RgbColor::from)
                .or(self.host_background)
                .unwrap_or(DEFAULT_BACKGROUND),
            foreground: colors[NamedColor::Foreground]
                .map(RgbColor::from)
                .or(self.host_foreground)
                .unwrap_or(DEFAULT_FOREGROUND),
            palette,
        }
    }

    fn render_cursor(&self) -> RenderCursor {
        let grid = self.term.grid();
        let point = grid.cursor.point;
        let display_offset = i64::try_from(grid.display_offset()).unwrap_or(i64::MAX);
        let viewport_y = i64::from(point.line.0) + display_offset;
        // Guarded by `viewport_y >= 0` below, so this never truncates in the branch that uses it.
        let viewport_y_usize = usize::try_from(viewport_y).unwrap_or(0);
        let viewport = (viewport_y >= 0
            && viewport_y_usize < grid.screen_lines()
            && point.column.0 < grid.columns())
        .then(|| CursorViewport {
            x: saturating_u16(point.column.0),
            y: saturating_u16(viewport_y_usize),
            wide_tail: grid[point].flags.contains(Flags::WIDE_CHAR_SPACER),
        });
        let style = self.term.cursor_style();
        RenderCursor {
            viewport,
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            blinking: style.blinking,
            visual_style: match style.shape {
                CursorShape::Block | CursorShape::Hidden => CursorVisualStyle::Block,
                CursorShape::Underline => CursorVisualStyle::Underline,
                CursorShape::Beam => CursorVisualStyle::Bar,
                CursorShape::HollowBlock => CursorVisualStyle::BlockHollow,
            },
        }
    }

    /// Folds alacritty's damage since the last call into our generation
    /// counters, then resets alacritty's tracking.
    fn collect_damage(&mut self) {
        let screen_lines = self.term.screen_lines();
        if self.row_damage_generations.len() != screen_lines {
            self.row_damage_generations = vec![0; screen_lines];
            self.bump_full_damage();
        }
        let next = self.damage_generation + 1;
        let mut damaged = false;
        match self.term.damage() {
            TermDamage::Full => {
                self.full_damage_generation = next;
                damaged = true;
            }
            TermDamage::Partial(lines) => {
                for bounds in lines {
                    if let Some(slot) = self.row_damage_generations.get_mut(bounds.line) {
                        *slot = next;
                        damaged = true;
                    }
                }
            }
        }
        self.term.reset_damage();
        if damaged {
            self.damage_generation = next;
        }
    }

    fn bump_full_damage(&mut self) {
        self.damage_generation += 1;
        self.full_damage_generation = self.damage_generation;
    }

    pub fn set_default_palette(&mut self, palette: &[RgbColor; 256]) -> Result<(), Error> {
        if self.default_palette != *palette {
            self.default_palette = *palette;
            self.bump_full_damage();
        }
        Ok(())
    }

    pub fn default_palette(&self) -> Result<[RgbColor; 256], Error> {
        Ok(self.default_palette)
    }

    /// Sets the host's default foreground/background (`None`: the built-in
    /// default). The child's own OSC 10/11 overrides stay on top of them, and
    /// OSC 110/111 fall back to them.
    pub fn set_default_colors(
        &mut self,
        foreground: Option<RgbColor>,
        background: Option<RgbColor>,
    ) {
        if (self.host_foreground, self.host_background) != (foreground, background) {
            self.host_foreground = foreground;
            self.host_background = background;
            self.bump_full_damage();
        }
    }

    /// The default colour the child set with OSC 10/11, if it has one.
    pub fn default_color_override(&self, color: DefaultColor) -> Option<RgbColor> {
        self.term.colors()[color.named()].map(RgbColor::from)
    }

    /// Drops the child's OSC 10/11 overrides, as OSC 110/111 would, so the
    /// host defaults show again. Goes through `Term`'s handler directly,
    /// never through the child's parser.
    pub fn reset_default_color_overrides(&mut self) {
        let mut changed = false;
        for color in [DefaultColor::Foreground, DefaultColor::Background] {
            if self.default_color_override(color).is_some() {
                Handler::reset_color(&mut self.term, color.named() as usize);
                changed = true;
            }
        }
        if changed {
            self.bump_full_damage();
        }
    }

    /// Whether the child set a default foreground or background since the
    /// last call.
    pub fn take_default_color_set(&mut self) -> bool {
        mem::take(&mut self.default_color_set)
    }

    /// The latest window-title change (OSC 0/2, CSI 23 t, RIS) since the last
    /// call: `Some(None)` is a reset, `None` means no change. The title is
    /// exactly what vte parsed (trimmed, not otherwise sanitised).
    pub fn take_title_update(&mut self) -> Option<Option<String>> {
        self.title_update.take()
    }

    /// The latest OSC 9;4 progress payload (the text after `9;`) since the
    /// last call.
    pub fn take_progress_update(&mut self) -> Option<Vec<u8>> {
        self.progress_update.take()
    }

    /// Whether the child chose a cursor shape (DECSCUSR 1-6 or OSC 50) that
    /// is still in effect.
    pub fn cursor_shape_overridden(&self) -> bool {
        self.modes.cursor_shape_set
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), Error> {
        let columns = usize::from(cols).max(MIN_COLUMNS);
        let screen_lines = usize::from(rows).max(1);
        let columns_changed = columns != self.term.columns();
        let lines_changed = screen_lines != self.term.screen_lines();
        let geometry_changed = columns_changed
            || lines_changed
            || cell_width_px != self.cell_width_px
            || cell_height_px != self.cell_height_px;

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
        self.cell_width_px = cell_width_px;
        self.cell_height_px = cell_height_px;
        self.drain_events();
        self.collect_damage();
        if geometry_changed && self.modes.in_band_resize {
            self.push_in_band_size_report();
        }
        Ok(())
    }

    fn set_history_lines(&mut self, history_lines: usize) {
        self.history_lines = history_lines;
        let queued = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        self.term.set_options(term_config(history_lines));
        // `set_options` re-announces the current title; that is not a change
        // the child made, so drop it.
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .truncate(queued);
    }

    pub fn set_color_scheme(&mut self, color_scheme: Option<ColorScheme>) -> Option<ColorScheme> {
        mem::replace(&mut self.color_scheme, color_scheme)
    }

    pub fn take_pwd_changes(&mut self) -> Vec<Vec<u8>> {
        mem::take(&mut self.pwd_changes)
    }

    pub fn take_clipboard_writes(&mut self) -> Vec<Vec<u8>> {
        mem::take(&mut self.clipboard_writes)
    }

    pub fn mode_get(&self, mode: u16) -> Result<bool, Error> {
        let term_mode = *self.term.mode();
        Ok(match mode {
            1 => term_mode.contains(TermMode::APP_CURSOR),
            6 => term_mode.contains(TermMode::ORIGIN),
            7 => term_mode.contains(TermMode::LINE_WRAP),
            9 => self.modes.x10_mouse,
            25 => term_mode.contains(TermMode::SHOW_CURSOR),
            47 | 1047 | 1049 => term_mode.contains(TermMode::ALT_SCREEN),
            1000 => term_mode.contains(TermMode::MOUSE_REPORT_CLICK),
            1002 => term_mode.contains(TermMode::MOUSE_DRAG),
            1003 => term_mode.contains(TermMode::MOUSE_MOTION),
            1004 => term_mode.contains(TermMode::FOCUS_IN_OUT),
            1005 => term_mode.contains(TermMode::UTF8_MOUSE),
            1006 => term_mode.contains(TermMode::SGR_MOUSE),
            1007 => term_mode.contains(TermMode::ALTERNATE_SCROLL),
            1016 => self.modes.sgr_pixels_mouse,
            2004 => term_mode.contains(TermMode::BRACKETED_PASTE),
            2026 => self.synchronized_output_deadline().is_some(),
            2031 => self.modes.color_scheme_report,
            2048 => self.modes.in_band_resize,
            _ => false,
        })
    }

    /// Sets a DEC private mode with the same effect as the child's
    /// `CSI ? mode h/l`, but through the handler directly: nothing is fed to
    /// the parser, so a sequence the child has half-written is not disturbed
    /// and a synchronized update does not defer it. Mode 2026 is refused: it
    /// is parser state, not terminal state.
    pub fn mode_set(&mut self, mode: u16, value: bool) -> Result<(), Error> {
        if mode == MODE_SYNCHRONIZED_OUTPUT {
            return Err(Error("synchronized output is driven by the parser"));
        }
        let private_mode = handler::private_mode(mode);
        self.rows.begin(&self.term);
        let mut handler = CoreHandler {
            term: &mut self.term,
            keyboard_depth: &mut self.keyboard_depth,
            modes: &mut self.modes,
            cell_width_px: self.cell_width_px,
            cell_height_px: self.cell_height_px,
            events: &self.events,
            default_color_set: &mut self.default_color_set,
            rows: &mut self.rows,
            history_limit: self.history_lines,
        };
        if value {
            Handler::set_private_mode(&mut handler, private_mode);
        } else {
            Handler::unset_private_mode(&mut handler, private_mode);
        }
        self.rows.finish(&self.term, self.history_lines);
        self.drain_events();
        self.collect_damage();
        Ok(())
    }

    /// Active kitty keyboard flags (bit 0 disambiguate … bit 4 associated text).
    pub fn kitty_keyboard_flags(&self) -> Result<u8, Error> {
        let term_mode = *self.term.mode();
        let mut flags = 0u8;
        for (mode, bit) in [
            (TermMode::DISAMBIGUATE_ESC_CODES, 0b0000_0001),
            (TermMode::REPORT_EVENT_TYPES, 0b0000_0010),
            (TermMode::REPORT_ALTERNATE_KEYS, 0b0000_0100),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 0b0000_1000),
            (TermMode::REPORT_ASSOCIATED_TEXT, 0b0001_0000),
        ] {
            if term_mode.contains(mode) {
                flags |= bit;
            }
        }
        Ok(flags)
    }

    pub fn mouse_tracking_enabled(&self) -> Result<bool, Error> {
        Ok(self.term.mode().intersects(TermMode::MOUSE_MODE) || self.modes.x10_mouse)
    }

    pub fn active_screen(&self) -> Result<ActiveScreen, Error> {
        Ok(if self.term.mode().contains(TermMode::ALT_SCREEN) {
            ActiveScreen::Alternate
        } else {
            ActiveScreen::Primary
        })
    }

    pub fn total_rows(&self) -> Result<usize, Error> {
        Ok(self.term.total_lines())
    }

    pub fn scrollback_rows(&self) -> Result<usize, Error> {
        Ok(self.term.history_size())
    }

    /// The configured scrollback budget in bytes.
    pub fn max_scrollback(&self) -> usize {
        self.max_scrollback
    }

    pub fn scrollbar(&self) -> Result<TerminalScrollbar, Error> {
        let history = self.term.history_size();
        Ok(TerminalScrollbar {
            total: self.term.total_lines(),
            offset: history.saturating_sub(self.term.grid().display_offset()),
            len: self.term.screen_lines(),
        })
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
    pub fn history_origin(&self) -> u64 {
        self.rows.origin()
    }

    /// The screen row of an absolute row id, `None` for a line that is no
    /// longer (or not yet) retained.
    pub fn screen_row_for_absolute(&self, row: u64) -> Option<usize> {
        let y = usize::try_from(row.checked_sub(self.rows.origin())?).ok()?;
        (y < self.term.total_lines()).then_some(y)
    }

    /// The absolute row id of screen row `y`.
    pub fn absolute_row_for_screen(&self, y: usize) -> u64 {
        self.rows
            .origin()
            .saturating_add(u64::try_from(y).unwrap_or(u64::MAX))
    }

    /// Visits the cells of screen row `y` without allocating: `visit` gets
    /// each cell's column, width class and text. The text is what readers
    /// show for the cell: its grapheme, or a single space for blank cells,
    /// wide-character spacers and kitty placeholder cells; callers skip
    /// `SpacerTail` cells where a wide character's second column must not
    /// produce text. `scratch` holds the text between calls. `None` when the
    /// row is not retained.
    pub(crate) fn visit_screen_row_text(
        &self,
        y: usize,
        scratch: &mut String,
        mut visit: impl FnMut(u16, CellWide, &str),
    ) -> Option<RowWrap> {
        let line = self.screen_line(u64::try_from(y).ok()?)?;
        let grid = self.term.grid();
        let columns = grid.columns();
        let last_column = Column(columns - 1);
        let row = &grid[line];
        for (x, cell) in row[..].iter().take(columns).enumerate() {
            let Ok(x) = u16::try_from(x) else {
                break;
            };
            cell_text_into(cell, scratch);
            visit(x, cell_wide(cell), scratch.as_str());
        }
        Some(RowWrap {
            soft_wrapped: row[last_column].flags.contains(Flags::WRAPLINE),
            wrap_continuation: line > grid.topmost_line()
                && grid[Line(line.0 - 1)][last_column]
                    .flags
                    .contains(Flags::WRAPLINE),
        })
    }

    /// Converts a screen row (0 = oldest retained line) to an alacritty line.
    fn screen_line(&self, y: u64) -> Option<Line> {
        let history_size = i64::try_from(self.term.history_size()).unwrap_or(i64::MAX);
        let line = i64::try_from(y).ok()? - history_size;
        let line = Line(i32::try_from(line).ok()?);
        (line >= self.term.topmost_line() && line <= self.term.bottommost_line()).then_some(line)
    }

    /// Converts a viewport row (0 = top of what is displayed) to an alacritty line.
    fn viewport_line(&self, y: u64) -> Option<Line> {
        let y = usize::try_from(y).ok()?;
        if y >= self.term.screen_lines() {
            return None;
        }
        let display_offset = i64::try_from(self.term.grid().display_offset()).unwrap_or(i64::MAX);
        let line = i64::try_from(y).unwrap_or(i64::MAX) - display_offset;
        Some(Line(i32::try_from(line).ok()?))
    }

    pub fn screen_cell(&self, x: u16, y: u32) -> Result<(CellWide, Vec<u32>), Error> {
        let line = self
            .screen_line(u64::from(y))
            .ok_or(Error("screen row out of range"))?;
        let column = usize::from(x);
        if column >= self.term.columns() {
            return Err(Error("screen column out of range"));
        }
        let cell = &self.term.grid()[line][Column(column)];
        Ok((cell_wide(cell), cell_graphemes(cell)))
    }

    pub(crate) fn screen_text_rows(&self) -> Result<Vec<ScreenTextRow>, Error> {
        self.screen_text_rows_range(0, usize::MAX)
    }

    pub(crate) fn screen_text_rows_range(
        &self,
        start_row: usize,
        end_row_exclusive: usize,
    ) -> Result<Vec<ScreenTextRow>, Error> {
        let total_rows = self.term.total_lines();
        let start_row = start_row.min(total_rows);
        let end_row_exclusive = end_row_exclusive.min(total_rows).max(start_row);
        let grid = self.term.grid();
        let columns = grid.columns();
        let last_column = Column(columns - 1);
        let mut rows = Vec::with_capacity(end_row_exclusive - start_row);
        for y in start_row..end_row_exclusive {
            let Some(line) = self.screen_line(y as u64) else {
                break;
            };
            let row = &grid[line];
            let cells = (0..columns)
                .map(|x| {
                    let cell = &row[Column(x)];
                    ScreenTextCell {
                        wide: cell_wide(cell),
                        graphemes: cell_graphemes(cell),
                    }
                })
                .collect();
            let soft_wrapped = row[last_column].flags.contains(Flags::WRAPLINE);
            let wrap_continuation = line > grid.topmost_line()
                && grid[Line(line.0 - 1)][last_column]
                    .flags
                    .contains(Flags::WRAPLINE);
            rows.push(ScreenTextRow {
                cells,
                soft_wrapped,
                wrap_continuation,
            });
        }
        Ok(rows)
    }

    pub fn viewport_hyperlink_uri(&self, x: u16, y: u32) -> Result<Option<String>, Error> {
        let line = self
            .viewport_line(u64::from(y))
            .ok_or(Error("viewport row out of range"))?;
        let column = usize::from(x);
        if column >= self.term.columns() {
            return Err(Error("viewport column out of range"));
        }
        Ok(self.term.grid()[line][Column(column)]
            .hyperlink()
            .map(|link| link.uri().to_owned()))
    }

    pub fn read_text_viewport(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start,
            end,
            Coordinates::Viewport,
            rectangle,
            Format::Plain,
            true,
        )
    }

    pub fn read_ansi_viewport(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start,
            end,
            Coordinates::Viewport,
            rectangle,
            Format::Vt,
            false,
        )
    }

    pub fn read_text_screen(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start,
            end,
            Coordinates::Screen,
            rectangle,
            Format::Plain,
            true,
        )
    }

    pub fn read_ansi_screen(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
        unwrap: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start,
            end,
            Coordinates::Screen,
            rectangle,
            Format::Vt,
            unwrap,
        )
    }

    fn read_range(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        coordinates: Coordinates,
        rectangle: bool,
        format: Format,
        unwrap: bool,
    ) -> Result<String, Error> {
        let to_line = |y: u32| match coordinates {
            Coordinates::Screen => self.screen_line(u64::from(y)),
            Coordinates::Viewport => self.viewport_line(u64::from(y)),
        };
        let grid = self.term.grid();
        let start = to_line(start.1)
            .and_then(|line| format::grid_point(grid, line, start.0))
            .ok_or(Error("selection start out of range"))?;
        let end = to_line(end.1)
            .and_then(|line| format::grid_point(grid, line, end.0))
            .ok_or(Error("selection end out of range"))?;
        Ok(format::format_range(
            grid, start, end, rectangle, format, unwrap, true,
        ))
    }

    /// Clears the screen and scrollback but keeps the cursor's (possibly
    /// soft-wrapped) line, moved to the top of the screen together with any
    /// DECSC-saved cursor position. Cleared rows are blank in default colours,
    /// whatever SGR the child has active. A no-op returning
    /// `false` while the alternate screen is active: the full-screen app owns
    /// that screen, and the primary history must survive until it exits.
    pub fn clear_screen(&mut self) -> bool {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return false;
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
        true
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
    pub fn scroll_viewport_row(&mut self, row: usize) {
        let history = self.term.history_size();
        let target_offset = history - row.min(history);
        let current = self.term.grid().display_offset();
        let target_offset_i64 = i64::try_from(target_offset).unwrap_or(i64::MAX);
        let current_i64 = i64::try_from(current).unwrap_or(i64::MAX);
        let delta = i32::try_from(
            (target_offset_i64 - current_i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)),
        )
        .unwrap_or(i32::MAX);
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
            self.collect_damage();
        }
    }

    pub fn cols(&self) -> Result<u16, Error> {
        Ok(saturating_u16(self.term.columns()))
    }

    pub fn rows(&self) -> Result<u16, Error> {
        Ok(saturating_u16(self.term.screen_lines()))
    }

    pub fn cursor_y(&self) -> Result<u16, Error> {
        let line = self.term.grid().cursor.point.line.0.max(0);
        Ok(u16::try_from(line).unwrap_or(u16::MAX))
    }

    /// The foreground in effect: the child's OSC 10 override, else the host
    /// default; `None` while neither is set.
    pub fn effective_foreground_color(&self) -> Result<Option<RgbColor>, Error> {
        Ok(self
            .default_color_override(DefaultColor::Foreground)
            .or(self.host_foreground))
    }

    /// The cursor colour set with OSC 12, if any.
    pub fn effective_cursor_color(&self) -> Result<Option<RgbColor>, Error> {
        Ok(self.term.colors()[NamedColor::Cursor].map(RgbColor::from))
    }

    pub(crate) fn width_px(&self) -> Result<u32, Error> {
        Ok(u32::try_from(self.term.columns())
            .unwrap_or(u32::MAX)
            .saturating_mul(self.cell_width_px))
    }

    pub(crate) fn height_px(&self) -> Result<u32, Error> {
        Ok(u32::try_from(self.term.screen_lines())
            .unwrap_or(u32::MAX)
            .saturating_mul(self.cell_height_px))
    }
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn cell_wide(cell: &Cell) -> CellWide {
    if cell.flags.contains(Flags::WIDE_CHAR) {
        CellWide::Wide
    } else if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        CellWide::SpacerTail
    } else if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        CellWide::SpacerHead
    } else {
        CellWide::Narrow
    }
}

fn cell_zerowidth(cell: &Cell) -> &[char] {
    cell.zerowidth().unwrap_or(&[])
}

/// The cell's text as codepoints; empty for blank cells and spacers.
fn cell_graphemes(cell: &Cell) -> Vec<u32> {
    if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        return Vec::new();
    }
    let zerowidth = cell_zerowidth(cell);
    if (cell.c == ' ' || cell.c == '\t') && zerowidth.is_empty() {
        return Vec::new();
    }
    let base = if cell.c == '\t' { ' ' } else { cell.c };
    let mut graphemes = Vec::with_capacity(1 + zerowidth.len());
    graphemes.push(u32::from(base));
    graphemes.extend(zerowidth.iter().map(|&ch| u32::from(ch)));
    graphemes
}

/// The cell's text as readers show it, into `out`: the grapheme, or a single
/// space for blank cells, wide-character spacers and kitty placeholders (the
/// same text [`cell_graphemes`] yields once empty cells read as a space).
fn cell_text_into(cell: &Cell, out: &mut String) {
    out.clear();
    if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        || u32::from(cell.c) == KITTY_UNICODE_PLACEHOLDER
    {
        out.push(' ');
        return;
    }
    out.push(if cell.c == '\t' { ' ' } else { cell.c });
    out.extend(cell_zerowidth(cell).iter().copied());
}

fn cell_color(color: Color) -> Option<CellColor> {
    match color {
        Color::Named(named) => {
            let index = named as usize;
            (index < 16).then(|| CellColor::Palette(u8::try_from(index).unwrap_or(u8::MAX)))
        }
        Color::Indexed(index) => Some(CellColor::Palette(index)),
        Color::Spec(rgb) => Some(CellColor::Rgb(rgb.into())),
    }
}

fn resolve_cell_color(color: CellColor, colors: &RenderColors) -> RgbColor {
    match color {
        CellColor::Palette(index) => colors.palette[usize::from(index)],
        CellColor::Rgb(rgb) => rgb,
    }
}

fn cell_style(cell: &Cell) -> CellStyle {
    let flags = cell.flags;
    let underline = if flags.contains(Flags::UNDERLINE) {
        1
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        2
    } else if flags.contains(Flags::UNDERCURL) {
        3
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        4
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        5
    } else {
        0
    };
    CellStyle {
        fg_color: cell_color(cell.fg),
        bg_color: cell_color(cell.bg),
        underline_color: cell.underline_color().and_then(cell_color),
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        faint: flags.contains(Flags::DIM),
        blink: false,
        inverse: flags.contains(Flags::INVERSE),
        invisible: flags.contains(Flags::HIDDEN),
        strikethrough: flags.contains(Flags::STRIKEOUT),
        overline: false,
        underline,
        underlined: underline != 0,
    }
}

#[derive(Default)]
struct RowSnapshot {
    cells: Vec<Cell>,
    dirty: std::cell::Cell<bool>,
}

/// A snapshot of the viewport for rendering. Row dirty flags accumulate across
/// [`RenderState::update`] calls until the caller clears them, mirroring the
/// libghostty-vt render-state contract the pane layer was written against.
pub struct RenderState {
    cols: usize,
    rows: Vec<RowSnapshot>,
    seen_generation: u64,
    dirty: Dirty,
    cursor: RenderCursor,
    colors: RenderColors,
}

impl RenderState {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            cols: 0,
            rows: Vec::new(),
            seen_generation: 0,
            dirty: Dirty::Clean,
            cursor: RenderCursor {
                viewport: None,
                visible: true,
                blinking: false,
                visual_style: CursorVisualStyle::Block,
            },
            colors: RenderColors {
                background: DEFAULT_BACKGROUND,
                foreground: DEFAULT_FOREGROUND,
                palette: default_palette(),
            },
        })
    }

    pub fn update(&mut self, terminal: &Terminal) -> Result<(), Error> {
        let grid = terminal.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let display_offset = grid.display_offset();
        let dims_changed = cols != self.cols || rows != self.rows.len();
        if dims_changed {
            self.cols = cols;
            self.rows = (0..rows).map(|_| RowSnapshot::default()).collect();
        }
        let full = dims_changed || terminal.full_damage_generation > self.seen_generation;
        let mut any_changed = false;
        for (y, snapshot) in self.rows.iter_mut().enumerate() {
            let changed = full
                || terminal
                    .row_damage_generations
                    .get(y)
                    .is_some_and(|generation| *generation > self.seen_generation);
            if !changed {
                continue;
            }
            let y_i32 = i32::try_from(y).unwrap_or(i32::MAX);
            let display_offset_i32 = i32::try_from(display_offset).unwrap_or(i32::MAX);
            let line = Line(y_i32 - display_offset_i32);
            let current = &grid[line][..];
            // alacritty damages the cursor row on every damage read, content
            // change or not, so a mode-only write would otherwise dirty it.
            if !full && snapshot.cells.as_slice() == current {
                continue;
            }
            snapshot.cells.clear();
            snapshot.cells.extend_from_slice(current);
            snapshot.dirty.set(true);
            any_changed = true;
        }
        if full {
            self.dirty = Dirty::Full;
        } else if any_changed && self.dirty == Dirty::Clean {
            self.dirty = Dirty::Partial;
        }
        self.seen_generation = terminal.damage_generation;
        self.cursor = terminal.render_cursor();
        self.colors = terminal.render_colors();
        Ok(())
    }

    pub fn cols(&self) -> Result<u16, Error> {
        Ok(saturating_u16(self.cols))
    }

    pub fn rows(&self) -> Result<u16, Error> {
        Ok(saturating_u16(self.rows.len()))
    }

    pub fn dirty(&self) -> Result<Dirty, Error> {
        Ok(self.dirty)
    }

    pub fn cursor(&self) -> Result<RenderCursor, Error> {
        Ok(self.cursor)
    }

    pub fn colors(&self) -> Result<RenderColors, Error> {
        Ok(self.colors)
    }

    pub fn clean(&mut self) -> Result<(), Error> {
        self.dirty = Dirty::Clean;
        for row in &self.rows {
            row.dirty.set(false);
        }
        Ok(())
    }

    pub fn set_dirty(&mut self, dirty: Dirty) -> Result<(), Error> {
        self.dirty = dirty;
        Ok(())
    }

    pub fn populate_row_iterator<'a>(
        &'a self,
        iterator: &'a mut RowIterator,
    ) -> Result<RowIter<'a>, Error> {
        let _ = iterator;
        Ok(RowIter {
            state: self,
            index: None,
            _iterator: PhantomData,
        })
    }
}

/// Reusable iterator handle (kept for API compatibility; holds no state).
pub struct RowIterator {
    _private: (),
}

impl RowIterator {
    pub fn new() -> Result<Self, Error> {
        Ok(Self { _private: () })
    }
}

pub struct RowIter<'a> {
    state: &'a RenderState,
    index: Option<usize>,
    _iterator: PhantomData<&'a mut RowIterator>,
}

impl<'a> RowIter<'a> {
    pub fn next(&mut self) -> bool {
        let next = self.index.map_or(0, |index| index + 1);
        self.index = Some(next);
        next < self.state.rows.len()
    }

    /// Advances to the next row that is dirty (every row counts while the
    /// whole state is `Dirty::Full`) and returns its viewport row.
    pub fn next_dirty(&mut self) -> Option<u16> {
        let rows = &self.state.rows;
        let mut next = self.index.map_or(0, |index| index + 1);
        while next < rows.len() {
            if self.state.dirty == Dirty::Full || rows[next].dirty.get() {
                self.index = Some(next);
                return Some(saturating_u16(next));
            }
            next += 1;
        }
        self.index = Some(rows.len());
        None
    }

    fn current_row(&self) -> Result<&'a RowSnapshot, Error> {
        let state = self.state;
        self.index
            .and_then(|index| state.rows.get(index))
            .ok_or(Error("row iterator is not positioned on a row"))
    }

    pub fn dirty(&self) -> Result<bool, Error> {
        Ok(self.current_row()?.dirty.get())
    }

    pub fn clear_dirty(&mut self) -> Result<(), Error> {
        self.set_dirty(false)
    }

    pub fn set_dirty(&mut self, dirty: bool) -> Result<(), Error> {
        self.current_row()?.dirty.set(dirty);
        Ok(())
    }

    /// Core-side selection is never used; shepr draws its own.
    pub fn selection(&self) -> Result<Option<RowSelection>, Error> {
        Ok(None)
    }

    pub fn populate_cells<'b>(
        &'b mut self,
        cells: &'b mut RowCells,
    ) -> Result<RowCellIter<'b>, Error> {
        let _ = cells;
        let state: &'a RenderState = self.state;
        let row = self.current_row()?;
        Ok(RowCellIter {
            cells: &row.cells,
            colors: &state.colors,
            position: None,
        })
    }
}

/// Reusable cell handle (kept for API compatibility; holds no state).
pub struct RowCells {
    _private: (),
}

impl RowCells {
    pub fn new() -> Result<Self, Error> {
        Ok(Self { _private: () })
    }
}

pub struct RowCellIter<'a> {
    cells: &'a [Cell],
    colors: &'a RenderColors,
    position: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellBasicData {
    pub wide: CellWide,
    pub has_hyperlink: bool,
    pub has_styling: bool,
    pub style: CellStyle,
}

impl Default for CellBasicData {
    fn default() -> Self {
        Self {
            wide: CellWide::Narrow,
            has_hyperlink: false,
            has_styling: false,
            style: CellStyle::default(),
        }
    }
}

impl<'a> RowCellIter<'a> {
    pub fn next(&mut self) -> bool {
        let next = self.position.map_or(0, |position| position + 1);
        self.position = Some(next);
        next < self.cells.len()
    }

    pub fn select(&mut self, x: u16) -> Result<(), Error> {
        let x = usize::from(x);
        if x >= self.cells.len() {
            return Err(Error("cell column out of range"));
        }
        self.position = Some(x);
        Ok(())
    }

    fn cell(&self) -> Result<&'a Cell, Error> {
        let cells = self.cells;
        self.position
            .and_then(|position| cells.get(position))
            .ok_or(Error("cell iterator is not positioned on a cell"))
    }

    pub fn basic_data(&self) -> Result<CellBasicData, Error> {
        let cell = self.cell()?;
        let style = cell_style(cell);
        Ok(CellBasicData {
            wide: cell_wide(cell),
            has_hyperlink: cell.hyperlink().is_some(),
            has_styling: style != CellStyle::default(),
            style,
        })
    }

    pub fn wide(&self) -> Result<CellWide, Error> {
        Ok(cell_wide(self.cell()?))
    }

    pub fn has_hyperlink(&self) -> Result<bool, Error> {
        Ok(self.cell()?.hyperlink().is_some())
    }

    pub fn style(&self) -> Result<CellStyle, Error> {
        Ok(cell_style(self.cell()?))
    }

    /// Background-only content cells are a libghostty concept; alacritty keeps
    /// fills in the cell style, so this is always `None`.
    pub fn content_bg_color(&self) -> Result<Option<CellColor>, Error> {
        Ok(None)
    }

    /// The cell's explicit foreground resolved to RGB; `None` for default.
    pub fn fg_color(&self) -> Result<Option<RgbColor>, Error> {
        let cell = self.cell()?;
        Ok(cell_color(cell.fg).map(|color| resolve_cell_color(color, self.colors)))
    }

    /// The cell's explicit background resolved to RGB; `None` for default.
    pub fn bg_color(&self) -> Result<Option<RgbColor>, Error> {
        let cell = self.cell()?;
        Ok(cell_color(cell.bg).map(|color| resolve_cell_color(color, self.colors)))
    }

    pub fn grapheme_text(&self) -> Result<String, Error> {
        let mut bytes = Vec::new();
        let mut text = String::new();
        self.grapheme_text_into(&mut bytes, &mut text)?;
        Ok(text)
    }

    /// Writes the cell's grapheme into `text` (empty for blank cells and wide
    /// spacers). `bytes` is scratch space kept for API compatibility.
    pub fn grapheme_text_into(&self, bytes: &mut Vec<u8>, text: &mut String) -> Result<(), Error> {
        text.clear();
        bytes.clear();
        let cell = self.cell()?;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            return Ok(());
        }
        let zerowidth = cell_zerowidth(cell);
        if (cell.c == ' ' || cell.c == '\t') && zerowidth.is_empty() {
            return Ok(());
        }
        text.push(if cell.c == '\t' { ' ' } else { cell.c });
        text.extend(zerowidth.iter().copied());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
