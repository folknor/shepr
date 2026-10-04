//! The emulator proper: alacritty's `Term`, the vte parser that drives it, and
//! the adapter state that must track both exactly (keyboard-mode stack depths
//! and the modes alacritty does not model).

use std::cell::Cell as ClockCell;
use std::time::Instant;

use alacritty_terminal::term::Term;
use vte::ansi::{Processor, Timeout};

use crate::effects::Listener;
use crate::handler::KeyboardStackDepth;
use crate::{ModifyOtherKeysLevel, TermSize, term_config};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ExtraModes {
    pub(super) x10_mouse: bool,
    pub(super) sgr_pixels_mouse: bool,
    pub(super) color_scheme_report: bool,
    pub(super) in_band_resize: bool,
    /// xterm modifyOtherKeys level.
    pub(super) modify_other_keys: ModifyOtherKeysLevel,
    /// The child chose a cursor shape (DECSCUSR 1-6 or OSC 50) and has not
    /// asked for the default back (DECSCUSR 0, RIS).
    pub(super) cursor_shape_set: bool,
    /// Between vte's BSU and ESU (or timeout) as the handler sees them, which
    /// inside a buffered frame is replay order. Only DECRQM ?2026 reads it;
    /// `Terminal::sync_update_buffering` (and so `Terminal::mode_get`) asks the
    /// parser instead. The two are different notions and are named apart so
    /// nobody unifies them.
    pub(super) sync_update_in_replay: bool,
}

/// VTE calls `set_timeout` while parsing BSU, but its default handler reads
/// the process clock there. The caller sets `now` before each parser advance;
/// `Processor::sync_timeout` exposes only a shared reference, so the adapter
/// uses cells for the caller's clock and VTE's timeout state. The deadline is
/// the authority for both runtime expiry and VTE buffering, so a deadline
/// that cannot be represented never leaves output buffered without an expiry.
#[derive(Debug, Default)]
pub(super) struct SyncUpdateTimeout {
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

pub(super) struct Emulator {
    pub(super) term: Term<Listener>,
    pub(super) parser: Processor<SyncUpdateTimeout>,
    /// Mirror of alacritty's keyboard-mode stack depths; the parser must only
    /// ever drive `term` through a `CoreHandler` so it stays exact.
    pub(super) keyboard_depth: KeyboardStackDepth,
    pub(super) modes: ExtraModes,
}

impl Emulator {
    pub(super) fn new(
        history_lines: usize,
        columns: usize,
        screen_lines: usize,
        listener: Listener,
    ) -> Self {
        let mut term = Term::new(
            term_config(history_lines),
            &TermSize {
                columns,
                screen_lines,
            },
            listener,
        );
        // alacritty starts fully damaged; our own generation counters already
        // start "full", so begin alacritty's tracking from a clean slate.
        term.reset_damage();
        Self {
            term,
            parser: Processor::new(),
            keyboard_depth: KeyboardStackDepth::default(),
            modes: ExtraModes::default(),
        }
    }

    /// Sets the caller's clock for the synchronized-update deadline the next
    /// parser advance may arm.
    pub(super) fn set_clock(&self, now: Instant) {
        self.parser.sync_timeout().set_now(now);
    }

    /// When the pending synchronized update will be force-ended, if one is active.
    pub(super) fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().deadline()
    }
}
