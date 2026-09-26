//! Absolute row addressing for the primary screen.
//!
//! Screen rows (0 = the oldest retained line) do not stay attached to their
//! lines: once the primary screen's history is at its line limit, every new
//! line of output evicts the oldest one, and screen row N then names the line
//! that used to be row N + 1. [`RowOrigin`] counts the lines that have left
//! the top of the retained buffer, so `origin + screen row` is an absolute row
//! id that names the same line for as long as that line is retained and is
//! never reused for another one.
//!
//! alacritty keeps no such count, and most of its scrolling happens inside
//! `Term` where no `Handler` call sees it (line wrapping, `ED 2` pushing the
//! screen into history). The tracker follows one physical row across each
//! batch of changes instead. alacritty keeps rows in a ring buffer: scrolling
//! rotates the ring, recycled rows are reset in place, and only a resize or a
//! history purge reallocates or frees a row's cells. The address of a row's
//! cell buffer is therefore a stable identity while the row is retained.
//! History rows only ever move together - scrolling pushes them up, eviction
//! drops them off the top - so where a history row ends up after the batch
//! says exactly how many lines were evicted.
//!
//! Purges are not observable that way (freed rows can be reallocated at the
//! same address), so the handler settles the count around them and adds the
//! dropped lines itself: `ED 3` ([`RowOrigin::evict`]), RIS and column
//! changes, which re-wrap every line ([`RowOrigin::invalidate_primary`]).
//!
//! The alternate screen has no history. Its rows are viewport rows, lines it
//! scrolls away are simply gone, and the origin does not move while it is
//! active. A primary screen without scrollback (a zero byte budget) is the
//! same: nothing identifies a line once it has scrolled off, so the origin
//! stays put and its rows behave like viewport rows too.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::{Term, TermMode};

#[derive(Debug, Default)]
pub(super) struct RowOrigin {
    /// Lines that have left the top of the primary screen's retained buffer
    /// since the terminal was created: the absolute id of screen row 0.
    evicted: u64,
    /// The row followed across the batch in progress, if one is.
    anchor: Option<Anchor>,
    /// Rows the primary grid held when last observed while active. RIS on the
    /// alternate screen discards the (then inactive) primary grid, which
    /// alacritty offers no way to measure at that point.
    primary_total: usize,
}

#[derive(Debug, Clone, Copy)]
struct Anchor {
    /// Address of the row's cell buffer.
    identity: usize,
    /// Content fingerprint of the row, checked alongside `identity` to catch
    /// a physical slot that got recycled for a different line (see
    /// [`row_signature`]).
    signature: u64,
    /// The row's screen row when the batch began.
    screen_row: usize,
    /// Rows the primary grid held when the batch began.
    total: usize,
}

fn primary_active<T>(term: &Term<T>) -> bool {
    !term.mode().contains(TermMode::ALT_SCREEN)
}

fn row_identity<T>(term: &Term<T>, line: Line) -> usize {
    term.grid()[line][..].as_ptr().addr()
}

/// A cheap content fingerprint for a row, used to reject an identity match
/// that is really a recycled physical slot: once a batch has scrolled more
/// lines through the ring buffer than it holds, every address is bound to
/// reappear somewhere in the scan below even though the line it originally
/// named is long gone, since alacritty's history storage never frees a row's
/// backing allocation, it only reassigns which logical line it represents.
/// Content differing at the same address means the row was overwritten, so
/// the anchor did not survive.
fn row_signature<T>(term: &Term<T>, line: Line) -> u64 {
    let row = &term.grid()[line];
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for cell in row {
        hash ^= u64::from(cell.c);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl RowOrigin {
    /// The absolute id of screen row 0 (the oldest retained line).
    pub(super) fn origin(&self) -> u64 {
        self.evicted
    }

    /// Starts following the primary screen across a batch of changes that
    /// [`RowOrigin::finish`] closes. Constant time.
    pub(super) fn begin<T: EventListener>(&mut self, term: &Term<T>) {
        self.anchor = None;
        if !primary_active(term) {
            return;
        }
        let history = term.history_size();
        let total = term.total_lines();
        self.primary_total = total;
        // The newest history row is the one least likely to be evicted by
        // the batch. Without history, the top screen row is the best guess:
        // it is the next row scrolling pushes into history.
        let (line, screen_row) = if history > 0 {
            (Line(-1), history - 1)
        } else {
            (Line(0), 0)
        };
        self.anchor = Some(Anchor {
            identity: row_identity(term, line),
            signature: row_signature(term, line),
            screen_row,
            total,
        });
    }

    /// Counts the lines the batch since [`RowOrigin::begin`] evicted from the
    /// top of the primary screen's history. `history_limit` is the history's
    /// line limit as it stands now.
    ///
    /// Constant time unless the history is at its limit; then it walks one
    /// row per line the batch pushed into history, which the batch already
    /// paid for when it scrolled them.
    pub(super) fn finish<T: EventListener>(&mut self, term: &Term<T>, history_limit: usize) {
        let Some(anchor) = self.anchor.take() else {
            return;
        };
        if !primary_active(term) {
            // Screen switches settle the count before they happen, so this
            // only runs if one slipped past the handler. The primary rows
            // cannot be inspected any more: count all of them as gone.
            self.evict(anchor.total);
            return;
        }
        self.primary_total = term.total_lines();
        let history = term.history_size();
        // History never shrinks inside a batch except through the purges
        // the handler accounts for itself, so a history below its limit
        // cannot have lost a line. Without any scrollback nothing is
        // tracked (see the module docs).
        if history_limit == 0 || history < history_limit {
            return;
        }
        let (Ok(history_rows), Ok(anchor_row)) =
            (i32::try_from(history), i32::try_from(anchor.screen_row))
        else {
            self.evict(anchor.total);
            return;
        };
        // Unmoved, the anchor would sit on the same screen row; every evicted
        // line moves it one row further up. Walk up from there.
        let start = (anchor_row - history_rows).min(-1);
        for line in (-history_rows..=start).rev() {
            if row_identity(term, Line(line)) == anchor.identity {
                if row_signature(term, Line(line)) != anchor.signature {
                    // Same physical slot, different content: the batch
                    // wrapped the ring past this row already, so the address
                    // match is a coincidence and the anchor is long gone.
                    break;
                }
                let now = history_rows + line;
                self.evict(usize::try_from(anchor_row - now).unwrap_or(0));
                return;
            }
        }
        // The anchor itself was evicted: the batch pushed at least a whole
        // history's worth of lines. Every line retained when it began is
        // counted as gone, which leaves no earlier id naming a retained line
        // even if a few of the old screen rows did survive.
        self.evict(anchor.total);
    }

    /// Notes the primary grid's size after a change that was accounted for
    /// without a batch (see [`RowOrigin::invalidate_primary`]).
    pub(super) fn observe<T: EventListener>(&mut self, term: &Term<T>) {
        self.anchor = None;
        if primary_active(term) {
            self.primary_total = term.total_lines();
        }
    }

    /// Records `lines` lines dropped from the top of the primary screen by a
    /// purge the caller performed (`ED 3`, the host's clear).
    pub(super) fn evict(&mut self, lines: usize) {
        self.evicted = self
            .evicted
            .saturating_add(u64::try_from(lines).unwrap_or(u64::MAX));
    }

    /// Counts every line of the primary screen as gone: RIS, and reflows
    /// that re-wrap every line so that no earlier id may keep naming one.
    pub(super) fn invalidate_primary<T: EventListener>(&mut self, term: &Term<T>) {
        let total = if primary_active(term) {
            term.total_lines()
        } else {
            self.primary_total
        };
        self.evict(total);
    }
}
