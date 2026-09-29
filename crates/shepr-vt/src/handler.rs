//! The `Handler` the vte parser drives instead of `Term` itself.
//!
//! [`CoreHandler`] forwards every parser action to alacritty's `Term`
//! unchanged, except where the adapter has to step in:
//!
//! * `push_keyboard_mode`: the pinned alacritty checks the keyboard-mode stack
//!   against its 4096-entry cap and then evicts from the *title* stack
//!   (`self.title_stack.remove(0)` in `Term::push_keyboard_mode`). With no
//!   title pushed that panics ("removal index 0 < len 0"); with titles pushed
//!   the keyboard stack grows without bound. A child writing about 20 KB of
//!   `CSI > 1 u` would otherwise kill the pane's reader thread while it holds
//!   the core locks. The handler mirrors the stack depths and never lets a push
//!   reach alacritty's broken branch.
//! * Private modes 9 (X10 mouse), 1016 (SGR-pixel mouse), 2031 (colour-scheme
//!   reports) and 2048 (in-band resize), which alacritty ignores: set, reset
//!   and DECRQM-reported here, plus the mouse modes that cancel 9 and 1016.
//!   Setting or resetting any of 1000/1002/1003 ends X10 mode: xterm keeps
//!   one mouse-mode variable for all four.
//! * DECRQM for mode 2026, which alacritty always reports as reset. vte hands
//!   the handler BSU/ESU as `set/unset_private_mode(SyncUpdate)` and replays
//!   a buffered frame in byte order, so a query inside a frame learns that
//!   the update is still active.
//! * `reset_state` (RIS) also resets those adapter modes and modifyOtherKeys,
//!   the DECSCUSR override flag and the child's OSC 4/10/11/12 colour
//!   overrides (which alacritty keeps), and reports the title reset alacritty
//!   makes without an event.
//! * `ED 3`, RIS and the 1049 screen swap settle the absolute row accounting
//!   (`rows.rs`) around themselves: they purge rows or change the grid the
//!   tracker follows.
//! * `set_title`/`push_title`/`pop_title` reach `Term`, whose `Title` and
//!   `ResetTitle` events are the adapter's only title source; the pane never
//!   parses OSC 0/2 itself.
//! * `set_color` for the default foreground/background notes that the child
//!   took over a default colour (the pane tracks who owns the override).
//! * `set_cursor_style`/`set_cursor_shape` note whether the child chose a
//!   cursor shape (DECSCUSR 1-6 or OSC 50) or asked for the default
//!   (DECSCUSR 0, RIS).
//! * modifyOtherKeys (`CSI > 4 ; Pv m`, `CSI ? 4 m`), which alacritty does not
//!   model.
//! * `input` of U+FF9E/U+FF9F, printed in a cell of their own (see
//!   [`CoreHandler::input_halfwidth_voiced_mark`]).
//! * `CSI 14 t`, answered only with known pixel geometry and computed without
//!   alacritty's u16 multiplication (which overflows for large cell sizes).
//!
//! Handling these here rather than in the byte scanner (`scan.rs`) matters
//! because vte buffers everything inside a synchronized update (mode 2026) and
//! replays it at ESU: only effects dispatched through the handler happen in
//! byte order relative to alacritty's own, inside or outside such an update.
//! Replies go into the same event queue alacritty's `PtyWrite`s use, so they
//! interleave with DA/DSR/DECRQM answers in request order.
//!
//! Every `Handler` method is listed explicitly: the trait gives each one a
//! no-op default, so a method left out here would be silently dropped rather
//! than reach `Term`. When bumping `alacritty_terminal`, diff vte's `Handler`
//! trait against this impl.

use std::sync::Mutex;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::{Term, TermMode, color};
use vte::ansi::cursor_icon::CursorIcon;
use vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedColor, NamedPrivateMode,
    PrivateMode, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

use crate::limits::KEYBOARD_MODE_STACK_MAX_DEPTH;

use super::DecMode;
use super::ExtraModes;
use super::modes::{self, ExtraMode};
use super::rows::RowOrigin;
use shepr_core::geometry::{GridSize, PaneGeometry};

/// The vte private mode a write of `mode` goes through, from the mode table
/// (`PrivateMode::new` is private to vte). Adapter-stored and unlisted modes
/// stay `Unknown`, exactly as vte's parser would deliver them.
pub(super) fn private_mode(mode: DecMode) -> PrivateMode {
    match modes::lookup(mode).set {
        modes::Setter::Vte(named) => PrivateMode::Named(named),
        modes::Setter::Extra(_) => PrivateMode::Unknown(mode.number()),
    }
}

/// The in-band resize report (`CSI 48 ; rows ; cols ; height ; width t`),
/// `None` while no pixel geometry is known.
pub(super) fn in_band_size_report(geometry: PaneGeometry) -> Option<String> {
    let (width, height) = geometry.text_area_px()?;
    Some(format!(
        "\x1b[48;{};{};{height};{width}t",
        geometry.rows(),
        geometry.cols()
    ))
}

/// The `CSI 14 t` reply (`CSI 4 ; height ; width t`), `None` while no pixel
/// geometry is known. Its pixels use the same u16 limit as PTY winsize.
pub(super) fn text_area_pixels_report(geometry: PaneGeometry) -> Option<String> {
    let (width, height) = geometry.text_area_px()?;
    Some(format!("\x1b[4;{height};{width}t"))
}

pub(super) fn geometry_for_terminal(
    cols: usize,
    rows: usize,
    cell: Option<shepr_core::geometry::CellPx>,
) -> PaneGeometry {
    PaneGeometry {
        grid: GridSize::clamped_pane(
            u16::try_from(cols).unwrap_or(u16::MAX),
            u16::try_from(rows).unwrap_or(u16::MAX),
        ),
        cell,
    }
}

/// Depths of alacritty's two keyboard-mode stacks, mirrored from the parser
/// actions that change them.
///
/// alacritty keeps one stack per screen and swaps them together with the
/// `ALT_SCREEN` mode bit (`Term::swap_alt`), so the stack in use is always
/// the one belonging to the screen that bit names. Keying the counters by that
/// bit therefore stays exact however the swap was triggered. `RIS` clears
/// both. The counts assume `kitty_keyboard` is on, which `term_config`
/// always sets; with it off alacritty ignores pushes and pops entirely.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct KeyboardStackDepth {
    pub(super) primary: usize,
    pub(super) alternate: usize,
}

impl KeyboardStackDepth {
    fn active(&mut self, alternate_screen: bool) -> &mut usize {
        if alternate_screen {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }
}

pub(super) struct CoreHandler<'a, T: EventListener> {
    pub(super) term: &'a mut Term<T>,
    pub(super) keyboard_depth: &'a mut KeyboardStackDepth,
    pub(super) modes: &'a mut ExtraModes,
    pub(super) cell: Option<shepr_core::geometry::CellPx>,
    /// The queue alacritty's listener fills; adapter replies go in as
    /// `PtyWrite`s so they keep byte order with alacritty's.
    pub(super) events: &'a Mutex<Vec<Event>>,
    /// Set when the child sets the default foreground or background (OSC
    /// 10/11); the terminal hands it to the pane with
    /// `take_default_color_set`.
    pub(super) default_color_set: &'a mut bool,
    /// Absolute row accounting. The terminal opens a batch before handing
    /// the handler to the parser and closes it afterwards; the handler
    /// settles it around the actions that purge rows or swap screens.
    pub(super) rows: &'a mut RowOrigin,
    /// The primary screen's history line limit.
    pub(super) history_limit: usize,
}

impl<T: EventListener> CoreHandler<'_, T> {
    /// Closes the row-accounting batch in progress, so the action about to
    /// run is accounted for on its own.
    fn settle_rows(&mut self) {
        self.rows.finish(self.term, self.history_limit);
    }

    /// Opens a new row-accounting batch after such an action.
    fn resume_rows(&mut self) {
        self.rows.begin(self.term);
    }

    fn primary_screen_active(&self) -> bool {
        !self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    fn active_keyboard_depth(&mut self) -> &mut usize {
        let alternate_screen = self.term.mode().contains(TermMode::ALT_SCREEN);
        self.keyboard_depth.active(alternate_screen)
    }

    fn reply(&self, text: String) {
        super::lock_auxiliary(self.events).push(Event::PtyWrite(text));
    }

    /// The adapter-modelled state of a private mode alacritty does not know
    /// or misreports (2026), `None` for every other mode.
    fn adapter_private_mode(&self, mode: PrivateMode) -> Option<bool> {
        match mode {
            PrivateMode::Unknown(number) => {
                modes::extra_mode(number).map(|extra| extra.get(self.modes))
            }
            PrivateMode::Named(NamedPrivateMode::SyncUpdate) => {
                Some(self.modes.synchronized_update)
            }
            _ => None,
        }
    }

    /// Prints U+FF9E/U+FF9F in a cell of its own. unicode-width counts these
    /// Grapheme_Extend marks as zero-width, so alacritty would fold them into
    /// the previous cell, while wcwidth, xterm and the program writing them
    /// advance the cursor one column; left alone, every later cell on the
    /// line would sit one column left of where the program believes it is.
    fn input_halfwidth_voiced_mark(&mut self, mark: char) {
        // A plain width-1 print handles wrapping, insert mode and the SGR
        // template; the cell it wrote then gets the mark as its character.
        Handler::input(self.term, ' ');
        let grid = self.term.grid_mut();
        let point = grid.cursor.point;
        let column = if grid.cursor.input_needs_wrap {
            point.column
        } else {
            Column(point.column.0.saturating_sub(1))
        };
        grid[point.line][column].c = mark;
    }
}

#[warn(clippy::missing_trait_methods)]
impl<T: EventListener> Handler for CoreHandler<'_, T> {
    fn set_title(&mut self, title: Option<String>) {
        Handler::set_title(self.term, title);
    }

    /// `None` is DECSCUSR 0: back to the terminal's default cursor.
    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        self.modes.cursor_shape_set = style.is_some();
        Handler::set_cursor_style(self.term, style);
    }

    /// OSC 50 `CursorShape=`: an explicit shape, like DECSCUSR 1-6.
    fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.modes.cursor_shape_set = true;
        Handler::set_cursor_shape(self.term, shape);
    }

    fn input(&mut self, c: char) {
        if super::cell::is_halfwidth_voiced_mark_codepoint(u32::from(c)) {
            self.input_halfwidth_voiced_mark(c);
        } else {
            Handler::input(self.term, c);
        }
    }

    fn goto(&mut self, line: i32, col: usize) {
        Handler::goto(self.term, line, col);
    }

    fn goto_line(&mut self, line: i32) {
        Handler::goto_line(self.term, line);
    }

    fn goto_col(&mut self, col: usize) {
        Handler::goto_col(self.term, col);
    }

    fn insert_blank(&mut self, count: usize) {
        Handler::insert_blank(self.term, count);
    }

    fn move_up(&mut self, rows: usize) {
        Handler::move_up(self.term, rows);
    }

    fn move_down(&mut self, rows: usize) {
        Handler::move_down(self.term, rows);
    }

    fn identify_terminal(&mut self, intermediate: Option<char>) {
        Handler::identify_terminal(self.term, intermediate);
    }

    fn device_status(&mut self, arg: usize) {
        Handler::device_status(self.term, arg);
    }

    fn move_forward(&mut self, col: usize) {
        Handler::move_forward(self.term, col);
    }

    fn move_backward(&mut self, col: usize) {
        Handler::move_backward(self.term, col);
    }

    fn move_down_and_cr(&mut self, row: usize) {
        Handler::move_down_and_cr(self.term, row);
    }

    fn move_up_and_cr(&mut self, row: usize) {
        Handler::move_up_and_cr(self.term, row);
    }

    fn put_tab(&mut self, count: u16) {
        Handler::put_tab(self.term, count);
    }

    fn backspace(&mut self) {
        Handler::backspace(self.term);
    }

    fn carriage_return(&mut self) {
        Handler::carriage_return(self.term);
    }

    fn linefeed(&mut self) {
        Handler::linefeed(self.term);
    }

    fn bell(&mut self) {
        Handler::bell(self.term);
    }

    fn substitute(&mut self) {
        Handler::substitute(self.term);
    }

    fn newline(&mut self) {
        Handler::newline(self.term);
    }

    fn set_horizontal_tabstop(&mut self) {
        Handler::set_horizontal_tabstop(self.term);
    }

    fn scroll_up(&mut self, rows: usize) {
        Handler::scroll_up(self.term, rows);
    }

    fn scroll_down(&mut self, rows: usize) {
        Handler::scroll_down(self.term, rows);
    }

    fn insert_blank_lines(&mut self, count: usize) {
        Handler::insert_blank_lines(self.term, count);
    }

    fn delete_lines(&mut self, count: usize) {
        Handler::delete_lines(self.term, count);
    }

    fn erase_chars(&mut self, count: usize) {
        Handler::erase_chars(self.term, count);
    }

    fn delete_chars(&mut self, count: usize) {
        Handler::delete_chars(self.term, count);
    }

    fn move_backward_tabs(&mut self, count: u16) {
        Handler::move_backward_tabs(self.term, count);
    }

    fn move_forward_tabs(&mut self, count: u16) {
        Handler::move_forward_tabs(self.term, count);
    }

    fn save_cursor_position(&mut self) {
        Handler::save_cursor_position(self.term);
    }

    fn restore_cursor_position(&mut self) {
        Handler::restore_cursor_position(self.term);
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        Handler::clear_line(self.term, mode);
    }

    /// `ED 3` purges the primary screen's history: the purged lines are
    /// counted as evicted (their rows are freed, so the row tracker could not
    /// follow them).
    fn clear_screen(&mut self, mode: ClearMode) {
        if matches!(mode, ClearMode::Saved) && self.primary_screen_active() {
            self.settle_rows();
            let purged = self.term.history_size();
            Handler::clear_screen(self.term, mode);
            self.rows.evict(purged);
            self.resume_rows();
        } else {
            Handler::clear_screen(self.term, mode);
        }
    }

    fn clear_tabs(&mut self, mode: TabulationClearMode) {
        Handler::clear_tabs(self.term, mode);
    }

    fn set_tabs(&mut self, interval: u16) {
        Handler::set_tabs(self.term, interval);
    }

    fn reset_state(&mut self) {
        // RIS empties the primary screen's history and resets every visible
        // line (from the alternate screen too): no earlier row id names a
        // line any more.
        self.settle_rows();
        self.rows.invalidate_primary(self.term);
        Handler::reset_state(self.term);
        self.resume_rows();
        // alacritty keeps OSC 4/10/11/12 colour overrides across RIS; xterm
        // drops them with the rest of the terminal state. Only the child's
        // overrides live in these slots (host colours sit underneath them in
        // the adapter), so clearing them brings back exactly the host theme.
        for index in 0..color::COUNT {
            if self.term.colors()[index].is_some() {
                Handler::reset_color(self.term, index);
            }
        }
        // alacritty's RIS empties both keyboard-mode stacks.
        *self.keyboard_depth = KeyboardStackDepth::default();
        // A synchronized update belongs to vte's parser, which RIS does not
        // end, so the DECRQM ?2026 state survives it.
        *self.modes = ExtraModes {
            synchronized_update: self.modes.synchronized_update,
            ..ExtraModes::default()
        };
        // alacritty clears its title (and title stack) here without sending
        // an event; report the reset so the pane's title follows.
        super::lock_auxiliary(self.events).push(Event::ResetTitle);
    }

    fn reverse_index(&mut self) {
        Handler::reverse_index(self.term);
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        Handler::terminal_attribute(self.term, attr);
    }

    fn set_mode(&mut self, mode: Mode) {
        Handler::set_mode(self.term, mode);
    }

    fn unset_mode(&mut self, mode: Mode) {
        Handler::unset_mode(self.term, mode);
    }

    fn report_mode(&mut self, mode: Mode) {
        Handler::report_mode(self.term, mode);
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        if let PrivateMode::Unknown(number) = mode
            && let Some(extra) = modes::extra_mode(number)
        {
            extra.set(self.modes, true);
            match extra {
                ExtraMode::X10Mouse => {
                    // X10 mouse replaces the other tracking modes, as in xterm.
                    for other in [
                        NamedPrivateMode::ReportMouseClicks,
                        NamedPrivateMode::ReportCellMouseMotion,
                        NamedPrivateMode::ReportAllMouseMotion,
                    ] {
                        Handler::unset_private_mode(self.term, other.into());
                    }
                }
                ExtraMode::InBandResize => {
                    if let Some(report) = in_band_size_report(geometry_for_terminal(
                        self.term.columns(),
                        self.term.screen_lines(),
                        self.cell,
                    )) {
                        self.reply(report);
                    }
                }
                ExtraMode::SgrPixelsMouse => {
                    // 1016 and 1005 select mutually exclusive mouse encodings.
                    Handler::unset_private_mode(self.term, NamedPrivateMode::Utf8Mouse.into());
                }
                ExtraMode::ColorSchemeReport => {}
            }
            return;
        }
        match mode {
            PrivateMode::Named(
                NamedPrivateMode::ReportMouseClicks
                | NamedPrivateMode::ReportCellMouseMotion
                | NamedPrivateMode::ReportAllMouseMotion,
            ) => self.modes.x10_mouse = false,
            PrivateMode::Named(NamedPrivateMode::Utf8Mouse) => {
                self.modes.sgr_pixels_mouse = false;
            }
            PrivateMode::Named(NamedPrivateMode::SyncUpdate) => {
                self.modes.synchronized_update = true;
            }
            PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                // The row tracker follows the active grid; settle it before
                // the swap and pick it up again on the other side.
                self.settle_rows();
                Handler::set_private_mode(self.term, mode);
                self.resume_rows();
                return;
            }
            // Including 1006, the base SGR mode for 1016's pixel coordinates:
            // applications may resend it without disabling 1016.
            _ => {}
        }
        Handler::set_private_mode(self.term, mode);
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        if let PrivateMode::Unknown(number) = mode
            && let Some(extra) = modes::extra_mode(number)
        {
            extra.set(self.modes, false);
            return;
        }
        match mode {
            // xterm keeps one variable for 9/1000/1002/1003, so
            // resetting any of them turns X10 reporting off too.
            PrivateMode::Named(
                NamedPrivateMode::ReportMouseClicks
                | NamedPrivateMode::ReportCellMouseMotion
                | NamedPrivateMode::ReportAllMouseMotion,
            ) => self.modes.x10_mouse = false,
            PrivateMode::Named(NamedPrivateMode::SyncUpdate) => {
                self.modes.synchronized_update = false;
            }
            PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                self.settle_rows();
                Handler::unset_private_mode(self.term, mode);
                self.resume_rows();
                return;
            }
            _ => {}
        }
        Handler::unset_private_mode(self.term, mode);
    }

    /// alacritty answers "not recognised" for the adapter-modelled modes and
    /// "reset" for 2026 even inside a synchronized update.
    fn report_private_mode(&mut self, mode: PrivateMode) {
        match self.adapter_private_mode(mode) {
            Some(enabled) => {
                let state = if enabled { 1 } else { 2 };
                self.reply(format!("\x1b[?{};{state}$y", mode.raw()));
            }
            None => Handler::report_private_mode(self.term, mode),
        }
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        Handler::set_scrolling_region(self.term, top, bottom);
    }

    fn set_keypad_application_mode(&mut self) {
        Handler::set_keypad_application_mode(self.term);
    }

    fn unset_keypad_application_mode(&mut self) {
        Handler::unset_keypad_application_mode(self.term);
    }

    fn set_active_charset(&mut self, index: CharsetIndex) {
        Handler::set_active_charset(self.term, index);
    }

    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        Handler::configure_charset(self.term, index, charset);
    }

    fn set_color(&mut self, index: usize, color: Rgb) {
        if index == NamedColor::Foreground as usize || index == NamedColor::Background as usize {
            *self.default_color_set = true;
        }
        Handler::set_color(self.term, index, color);
    }

    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        Handler::dynamic_color_sequence(self.term, prefix, index, terminator);
    }

    fn reset_color(&mut self, index: usize) {
        Handler::reset_color(self.term, index);
    }

    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        Handler::clipboard_store(self.term, clipboard, base64);
    }

    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        Handler::clipboard_load(self.term, clipboard, terminator);
    }

    fn decaln(&mut self) {
        Handler::decaln(self.term);
    }

    fn push_title(&mut self) {
        Handler::push_title(self.term);
    }

    fn pop_title(&mut self) {
        Handler::pop_title(self.term);
    }

    /// Not forwarded: alacritty's reply closure multiplies u16 cell sizes.
    fn text_area_size_pixels(&mut self) {
        if let Some(report) = text_area_pixels_report(geometry_for_terminal(
            self.term.columns(),
            self.term.screen_lines(),
            self.cell,
        )) {
            self.reply(report);
        }
    }

    /// `CSI 18 t` reports characters, so it is answered with or without pixel
    /// geometry.
    fn text_area_size_chars(&mut self) {
        Handler::text_area_size_chars(self.term);
    }

    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        Handler::set_hyperlink(self.term, hyperlink);
    }

    fn set_mouse_cursor_icon(&mut self, icon: CursorIcon) {
        Handler::set_mouse_cursor_icon(self.term, icon);
    }

    fn report_keyboard_mode(&mut self) {
        Handler::report_keyboard_mode(self.term);
    }

    /// At the cap, replaces the top entry instead of pushing. The kitty spec
    /// asks for the oldest entry to be evicted, but alacritty offers no way to
    /// drop the bottom of its stack; its own attempt is the broken branch this
    /// avoids. Replacing the top keeps the newly requested mode active and the
    /// stack bounded, and only differs once a program has popped its way back
    /// down through thousands of entries.
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        if *self.active_keyboard_depth() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
            Handler::pop_keyboard_modes(self.term, 1);
            let depth = self.active_keyboard_depth();
            *depth = depth.saturating_sub(1);
        }
        Handler::push_keyboard_mode(self.term, mode);
        *self.active_keyboard_depth() += 1;
    }

    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        Handler::pop_keyboard_modes(self.term, to_pop);
        let depth = self.active_keyboard_depth();
        *depth = depth.saturating_sub(usize::from(to_pop));
    }

    fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        Handler::set_keyboard_mode(self.term, mode, behavior);
    }

    /// vte dispatches `CSI > 4 ; Pv m` for Pv 0..=2 (missing means 0). The
    /// spellings it drops (`CSI > m`, `CSI > 4 n`, Pv above 2) are still
    /// picked up by the byte scanner.
    fn set_modify_other_keys(&mut self, mode: ModifyOtherKeys) {
        self.modes.modify_other_keys = match mode {
            ModifyOtherKeys::Reset => super::ModifyOtherKeysLevel::Off,
            ModifyOtherKeys::EnableExceptWellDefined => {
                super::ModifyOtherKeysLevel::ExceptWellDefined
            }
            ModifyOtherKeys::EnableAll => super::ModifyOtherKeysLevel::All,
        };
        Handler::set_modify_other_keys(self.term, mode);
    }

    /// Answered here; the pinned alacritty leaves it a no-op.
    fn report_modify_other_keys(&mut self) {
        let level = self.modes.modify_other_keys;
        self.reply(format!("\x1b[>4;{level}m"));
    }

    fn set_scp(&mut self, char_path: ScpCharPath, update_mode: ScpUpdateMode) {
        Handler::set_scp(self.term, char_path, update_mode);
    }
}
