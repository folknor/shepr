//! Shepr's terminal-core boundary.
//!
//! The emulator underneath is `alacritty_terminal` (pinned in Cargo.toml). The
//! rest of the tree only sees the types in this module; alacritty types stay
//! private here. The module keeps the name `ghostty` from the libghostty-vt
//! era until the planned rename to `vt`.
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

mod format;
mod handler;
mod locks;
mod modes;
mod rows;
mod scan;
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

use crate::terminal::{AbsRow, Point, ScreenRow, ViewportRow};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub fn inferred_appearance(self) -> ColorScheme {
        let luminance = u32::from(self.r) * 299 + u32::from(self.g) * 587 + u32::from(self.b) * 114;
        if luminance >= 128_000 {
            ColorScheme::Light
        } else {
            ColorScheme::Dark
        }
    }
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

/// The terminal underline shape carried by a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg_color: Option<CellColor>,
    pub bg_color: Option<CellColor>,
    pub underline_color: Option<CellColor>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub underline: UnderlineStyle,
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
    // Kept flat for current snapshot constructors and readers; RowWrap owns
    // the shared calculation used to populate these two values.
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

fn is_halfwidth_voiced_mark(codepoint: u32) -> bool {
    matches!(codepoint, 0xff9e | 0xff9f)
}

pub fn unicode_codepoint_width(codepoint: u32) -> u8 {
    if is_halfwidth_voiced_mark(codepoint) {
        return 1;
    }
    match char::from_u32(codepoint) {
        Some(ch) => u8::try_from(ch.width().unwrap_or(0).min(2)).unwrap_or(2),
        None => 1,
    }
}

/// A visible text unit and the number of terminal cells it occupies.
/// Graphemes stay together, except that halfwidth voiced marks always get a
/// cell of their own as they do in [`CoreHandler`].
pub(crate) struct UnicodeDisplayUnits<'a> {
    graphemes: unicode_segmentation::Graphemes<'a>,
    remaining: &'a str,
}

impl<'a> Iterator for UnicodeDisplayUnits<'a> {
    type Item = (&'a str, u8);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            self.remaining = self.graphemes.next()?;
        }
        let special = self
            .remaining
            .char_indices()
            .find(|(_, character)| is_halfwidth_voiced_mark(*character as u32));
        if let Some((index, character)) = special {
            if index == 0 {
                let end = character.len_utf8();
                let mark = &self.remaining[..end];
                self.remaining = &self.remaining[end..];
                return Some((mark, unicode_codepoint_width(character as u32)));
            }
            let unit = &self.remaining[..index];
            self.remaining = &self.remaining[index..];
            return Some((unit, unicode_grapheme_cell_width(unit)));
        }
        let unit = self.remaining;
        self.remaining = "";
        Some((unit, unicode_grapheme_cell_width(unit)))
    }
}

fn unicode_grapheme_cell_width(grapheme: &str) -> u8 {
    use unicode_width::UnicodeWidthStr;

    if grapheme.chars().all(char::is_control) {
        0
    } else {
        u8::try_from(grapheme.width().min(2)).unwrap_or(2)
    }
}

/// Iterate text as terminal display units without allocating.
pub(crate) fn unicode_display_units(text: &str) -> UnicodeDisplayUnits<'_> {
    use unicode_segmentation::UnicodeSegmentation;

    UnicodeDisplayUnits {
        graphemes: text.graphemes(true),
        remaining: "",
    }
}

/// Width of text under the terminal grid's grapheme and voiced-mark rules.
pub fn unicode_text_width(text: &str) -> usize {
    unicode_display_units(text).fold(0usize, |width, (_, unit_width)| {
        width.saturating_add(usize::from(unit_width))
    })
}

/// Width of the first grapheme cluster in `codepoints`, returned as
/// `(codepoints consumed, cell width)`.
#[cfg(test)]
pub fn test_unicode_grapheme_width(codepoints: &[u32]) -> (usize, u8) {
    use unicode_segmentation::UnicodeSegmentation;

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
    let width = unicode_display_units(cluster).fold(0u8, |width, (_, unit_width)| {
        width.saturating_add(unit_width)
    });
    (consumed, width)
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
            crate::ghostty::lock_auxiliary(&self.0).push(event);
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

#[derive(Clone, Copy)]
enum Coordinates {
    Screen(ScreenRow),
    Viewport(ViewportRow),
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
            let mut queue = crate::ghostty::lock_auxiliary(&self.events);
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

    pub fn set_default_palette(&mut self, palette: &[RgbColor; 256]) {
        if self.default_palette != *palette {
            self.default_palette = *palette;
            self.bump_full_damage();
        }
    }

    pub fn default_palette(&self) -> [RgbColor; 256] {
        self.default_palette
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
        let queued = crate::ghostty::lock_auxiliary(&self.events).len();
        self.term.set_options(term_config(history_lines));
        // Term::set_options sends only the current title (or reset) through
        // this listener, synchronously. Term mutation is private to this
        // &mut Terminal API and no other event producer can reach this queue,
        // so truncating the tail drops only that synthetic reannouncement,
        // which is not a title change made by the child.
        crate::ghostty::lock_auxiliary(&self.events).truncate(queued);
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

    /// Active kitty keyboard flags (bit 0 disambiguate … bit 4 associated text).
    pub fn kitty_keyboard_flags(&self) -> crate::protocol::KittyKeyboardFlags {
        let term_mode = *self.term.mode();
        let mut flags = crate::protocol::KittyKeyboardFlags::NONE;
        for (mode, bit) in [
            (
                TermMode::DISAMBIGUATE_ESC_CODES,
                crate::protocol::KittyKeyboardFlags::DISAMBIGUATE,
            ),
            (
                TermMode::REPORT_EVENT_TYPES,
                crate::protocol::KittyKeyboardFlags::REPORT_EVENT_TYPES,
            ),
            (
                TermMode::REPORT_ALTERNATE_KEYS,
                crate::protocol::KittyKeyboardFlags::REPORT_ALTERNATE_KEYS,
            ),
            (
                TermMode::REPORT_ALL_KEYS_AS_ESC,
                crate::protocol::KittyKeyboardFlags::REPORT_ALL_KEYS,
            ),
            (
                TermMode::REPORT_ASSOCIATED_TEXT,
                crate::protocol::KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT,
            ),
        ] {
            if term_mode.contains(mode) {
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

    /// Visits the cells of screen row `y` without allocating: `visit` gets
    /// each cell's column, width class and text. The text is what readers
    /// show for the cell: its grapheme, or a single space for blank cells,
    /// wide-character spacers and kitty placeholder cells; callers skip
    /// `SpacerTail` cells where a wide character's second column must not
    /// produce text. `scratch` holds the text between calls. `None` when the
    /// row is not retained.
    pub(crate) fn visit_screen_row_text(
        &self,
        y: ScreenRow,
        scratch: &mut String,
        mut visit: impl FnMut(u16, CellWide, &str),
    ) -> Option<RowWrap> {
        let line = self.screen_line(y)?;
        let grid = self.term.grid();
        let columns = grid.columns();
        let row = &grid[line];
        for (x, cell) in row[..].iter().take(columns).enumerate() {
            let Ok(x) = u16::try_from(x) else {
                break;
            };
            cell_text_into(cell, scratch);
            visit(x, cell_wide(cell), scratch.as_str());
        }
        Some(self.row_wrap(line))
    }

    /// The single rule for the two wrap flags exposed by row readers.
    fn row_wrap(&self, line: Line) -> RowWrap {
        let grid = self.term.grid();
        let last_column = Column(grid.columns() - 1);
        RowWrap {
            soft_wrapped: grid[line][last_column].flags.contains(Flags::WRAPLINE),
            wrap_continuation: line > grid.topmost_line()
                && grid[Line(line.0 - 1)][last_column]
                    .flags
                    .contains(Flags::WRAPLINE),
        }
    }

    /// Converts a screen row (0 = oldest retained line) to an alacritty line.
    fn screen_line(&self, y: ScreenRow) -> Option<Line> {
        let history_size = i64::try_from(self.term.history_size()).unwrap_or(i64::MAX);
        let line = i64::try_from(y.0).ok()? - history_size;
        let line = Line(i32::try_from(line).ok()?);
        (line >= self.term.topmost_line() && line <= self.term.bottommost_line()).then_some(line)
    }

    /// Converts a viewport row (0 = top of what is displayed) to an alacritty line.
    fn viewport_line(&self, y: ViewportRow) -> Option<Line> {
        let y = usize::from(y.0);
        if y >= self.term.screen_lines() {
            return None;
        }
        let display_offset = i64::try_from(self.term.grid().display_offset()).unwrap_or(i64::MAX);
        let line = i64::try_from(y).unwrap_or(i64::MAX) - display_offset;
        Some(Line(i32::try_from(line).ok()?))
    }

    #[cfg(test)]
    pub fn screen_cell(&self, x: u16, y: ScreenRow) -> Result<(CellWide, Vec<u32>), Error> {
        let line = self
            .screen_line(y)
            .ok_or(Error("screen row out of range"))?;
        let column = usize::from(x);
        if column >= self.term.columns() {
            return Err(Error("screen column out of range"));
        }
        let cell = &self.term.grid()[line][Column(column)];
        Ok((cell_wide(cell), cell_graphemes(cell)))
    }

    pub(crate) fn screen_text_rows(&self) -> Vec<ScreenTextRow> {
        self.screen_text_rows_range(ScreenRow(0), ScreenRow(usize::MAX))
    }

    pub(crate) fn screen_text_rows_range(
        &self,
        start_row: ScreenRow,
        end_row_exclusive: ScreenRow,
    ) -> Vec<ScreenTextRow> {
        let total_rows = self.term.total_lines();
        let start_row = start_row.0.min(total_rows);
        let end_row_exclusive = end_row_exclusive.0.min(total_rows).max(start_row);
        let grid = self.term.grid();
        let columns = grid.columns();
        let mut rows = Vec::with_capacity(end_row_exclusive - start_row);
        for y in start_row..end_row_exclusive {
            let Some(line) = self.screen_line(ScreenRow(y)) else {
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
            let wrap = self.row_wrap(line);
            rows.push(ScreenTextRow {
                cells,
                soft_wrapped: wrap.soft_wrapped,
                wrap_continuation: wrap.wrap_continuation,
            });
        }
        rows
    }

    pub fn viewport_hyperlink_uri(&self, x: u16, y: ViewportRow) -> Result<Option<String>, Error> {
        let line = self
            .viewport_line(y)
            .ok_or(Error("viewport row out of range"))?;
        let column = usize::from(x);
        if column >= self.term.columns() {
            return Err(Error("viewport column out of range"));
        }
        Ok(self.term.grid()[line][Column(column)]
            .hyperlink()
            .map(|link| link.uri().to_owned()))
    }

    #[cfg(test)]
    pub fn read_text_viewport(
        &self,
        start: Point<ViewportRow>,
        end: Point<ViewportRow>,
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Viewport),
            end.map_row(Coordinates::Viewport),
            rectangle,
            Format::Plain,
            true,
        )
    }

    pub fn read_ansi_viewport(
        &self,
        start: Point<ViewportRow>,
        end: Point<ViewportRow>,
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Viewport),
            end.map_row(Coordinates::Viewport),
            rectangle,
            Format::Vt,
            false,
        )
    }

    pub fn read_text_screen(
        &self,
        start: Point<ScreenRow>,
        end: Point<ScreenRow>,
        rectangle: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Screen),
            end.map_row(Coordinates::Screen),
            rectangle,
            Format::Plain,
            true,
        )
    }

    pub fn read_ansi_screen(
        &self,
        start: Point<ScreenRow>,
        end: Point<ScreenRow>,
        rectangle: bool,
        unwrap: bool,
    ) -> Result<String, Error> {
        self.read_range(
            start.map_row(Coordinates::Screen),
            end.map_row(Coordinates::Screen),
            rectangle,
            Format::Vt,
            unwrap,
        )
    }

    fn read_range(
        &self,
        start: Point<Coordinates>,
        end: Point<Coordinates>,
        rectangle: bool,
        format: Format,
        unwrap: bool,
    ) -> Result<String, Error> {
        let to_line = |coordinate| match coordinate {
            Coordinates::Screen(y) => self.screen_line(y),
            Coordinates::Viewport(y) => self.viewport_line(y),
        };
        let grid = self.term.grid();
        let start = to_line(start.row)
            .and_then(|line| format::grid_point(grid, line, start.col))
            .ok_or(Error("selection start out of range"))?;
        let end = to_line(end.row)
            .and_then(|line| format::grid_point(grid, line, end.col))
            .ok_or(Error("selection end out of range"))?;
        Ok(format::format_range(
            grid, start, end, rectangle, format, unwrap, true,
        ))
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

enum CellText<'a> {
    Empty,
    Grapheme { base: char, zerowidth: &'a [char] },
}

/// One classification for the text-facing cell adapters. Empty cells,
/// spacers and kitty graphics placeholders all represent a blank cell.
fn cell_text(cell: &Cell) -> CellText<'_> {
    if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        || u32::from(cell.c) == KITTY_UNICODE_PLACEHOLDER
    {
        return CellText::Empty;
    }
    let zerowidth = cell_zerowidth(cell);
    if (cell.c == ' ' || cell.c == '\t') && zerowidth.is_empty() {
        return CellText::Empty;
    }
    CellText::Grapheme {
        base: if cell.c == '\t' { ' ' } else { cell.c },
        zerowidth,
    }
}

/// The cell's text as codepoints; empty for blank cells and spacers.
fn cell_graphemes(cell: &Cell) -> Vec<u32> {
    match cell_text(cell) {
        CellText::Empty => Vec::new(),
        CellText::Grapheme { base, zerowidth } => {
            let mut graphemes = Vec::with_capacity(1 + zerowidth.len());
            graphemes.push(u32::from(base));
            graphemes.extend(zerowidth.iter().map(|&ch| u32::from(ch)));
            graphemes
        }
    }
}

/// The cell's text as readers show it, into `out`: the grapheme, or a single
/// space for a cell classified as empty.
fn cell_text_into(cell: &Cell, out: &mut String) {
    out.clear();
    match cell_text(cell) {
        CellText::Empty => out.push(' '),
        CellText::Grapheme { base, zerowidth } => {
            out.push(base);
            out.extend(zerowidth.iter().copied());
        }
    }
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
        UnderlineStyle::Single
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineStyle::Double
    } else if flags.contains(Flags::UNDERCURL) {
        UnderlineStyle::Curly
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else {
        UnderlineStyle::None
    };
    CellStyle {
        fg_color: cell_color(cell.fg),
        bg_color: cell_color(cell.bg),
        underline_color: cell.underline_color().and_then(cell_color),
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        faint: flags.contains(Flags::DIM),
        inverse: flags.contains(Flags::INVERSE),
        invisible: flags.contains(Flags::HIDDEN),
        strikethrough: flags.contains(Flags::STRIKEOUT),
        underline,
    }
}

#[derive(Default)]
struct RowSnapshot {
    cells: Vec<Cell>,
    dirty: std::cell::Cell<bool>,
}

/// A snapshot of the viewport for rendering. Row dirty flags accumulate across
/// [`RenderState::update`] calls until the caller clears them.
pub struct RenderState {
    cols: usize,
    rows: Vec<RowSnapshot>,
    seen_generation: u64,
    dirty: Dirty,
    cursor: RenderCursor,
    colors: RenderColors,
}

impl RenderState {
    pub fn new() -> Self {
        Self {
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
        }
    }

    pub fn update(&mut self, terminal: &Terminal) {
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
    }

    #[cfg(test)]
    pub fn cols(&self) -> u16 {
        saturating_u16(self.cols)
    }

    #[cfg(test)]
    pub fn rows(&self) -> u16 {
        saturating_u16(self.rows.len())
    }

    pub fn dirty(&self) -> Dirty {
        self.dirty
    }

    pub fn cursor(&self) -> RenderCursor {
        self.cursor
    }

    pub fn colors(&self) -> RenderColors {
        self.colors
    }

    #[cfg(test)]
    pub fn clean(&mut self) {
        self.dirty = Dirty::Clean;
        for row in &self.rows {
            row.dirty.set(false);
        }
    }

    pub fn set_dirty(&mut self, dirty: Dirty) {
        self.dirty = dirty;
    }

    /// Iterates over every row as borrowed cell views.
    pub fn iter_rows(&self) -> Rows<'_> {
        Rows {
            state: self,
            next: 0,
            dirty_only: false,
        }
    }

    /// Iterates over changed rows without allocating or taking a lock.
    pub fn dirty_rows(&self) -> Rows<'_> {
        Rows {
            state: self,
            next: 0,
            dirty_only: true,
        }
    }
}

pub struct Rows<'a> {
    state: &'a RenderState,
    next: usize,
    dirty_only: bool,
}

impl<'a> Iterator for Rows<'a> {
    type Item = RowView<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.state.rows.len() {
            let index = self.next;
            self.next += 1;
            let snapshot = &self.state.rows[index];
            if self.dirty_only && self.state.dirty != Dirty::Full && !snapshot.dirty.get() {
                continue;
            }
            return Some(RowView {
                index,
                snapshot,
                colors: &self.state.colors,
            });
        }
        None
    }
}

pub struct RowView<'a> {
    index: usize,
    snapshot: &'a RowSnapshot,
    colors: &'a RenderColors,
}

impl<'a> RowView<'a> {
    pub fn y(&self) -> u16 {
        saturating_u16(self.index)
    }

    pub fn is_dirty(&self) -> bool {
        self.snapshot.dirty.get()
    }

    pub fn clear_dirty(&self) {
        self.snapshot.dirty.set(false);
    }

    pub fn cells(&self) -> impl Iterator<Item = CellView<'a>> + 'a {
        let colors = self.colors;
        self.snapshot
            .cells
            .iter()
            .map(move |cell| CellView { cell, colors })
    }
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

#[derive(Clone, Copy)]
pub struct CellView<'a> {
    cell: &'a Cell,
    colors: &'a RenderColors,
}

impl CellView<'_> {
    pub fn basic_data(&self) -> CellBasicData {
        let cell = self.cell;
        let style = cell_style(cell);
        CellBasicData {
            wide: cell_wide(cell),
            has_hyperlink: cell.hyperlink().is_some(),
            has_styling: style != CellStyle::default(),
            style,
        }
    }

    pub fn wide(&self) -> CellWide {
        cell_wide(self.cell)
    }

    pub fn has_hyperlink(&self) -> bool {
        self.cell.hyperlink().is_some()
    }

    /// The cell's explicit foreground resolved to RGB; `None` for default.
    pub fn fg_color(&self) -> Option<RgbColor> {
        cell_color(self.cell.fg).map(|color| resolve_cell_color(color, self.colors))
    }

    /// The cell's explicit background resolved to RGB; `None` for default.
    pub fn bg_color(&self) -> Option<RgbColor> {
        cell_color(self.cell.bg).map(|color| resolve_cell_color(color, self.colors))
    }

    pub fn grapheme_text(&self) -> String {
        let mut text = String::new();
        self.grapheme_text_into(&mut text);
        text
    }

    /// Writes the cell's grapheme into `text` (empty for blank cells and spacers).
    pub fn grapheme_text_into(&self, text: &mut String) {
        text.clear();
        match cell_text(self.cell) {
            CellText::Empty => {}
            CellText::Grapheme { base, zerowidth } => {
                text.push(base);
                text.extend(zerowidth.iter().copied());
            }
        }
    }
}

#[cfg(test)]
mod tests;
