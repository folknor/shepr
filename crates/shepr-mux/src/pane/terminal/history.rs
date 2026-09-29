//! Primary-screen history for persistence, read off the event loop.
//!
//! A saved pane history is the VT text of every retained primary-screen line.
//! Formatting all of it under one hold of the terminal lock would stall the
//! PTY reader, rendering and detection for as long as the scrollback takes to
//! format, and doing it on every save repeats work for lines that have not
//! changed since the last one. So a reader keeps what it formatted before
//! ([`PaneHistoryCache`]) and only formats what is new, one bounded chunk per
//! lock hold.
//!
//! What makes the cache sound is absolute row addressing
//! ([`shepr_vt::Terminal::history_origin`]): an absolute row names the same
//! line for as long as it is retained, and a line that has scrolled into
//! history does not change while it stays there. The exceptions are all
//! visible to the reader:
//!
//! - eviction from the top (history at its limit, `ED 3`, the host's clear)
//!   moves the origin past the evicted rows, and cached text for them is
//!   dropped;
//! - a column change and RIS re-wrap or discard every line, and move the
//!   origin past every earlier row;
//! - a taller pane pulls history rows back onto the screen, where the child
//!   can rewrite them. Every resize that changes the grid bumps the core's
//!   history epoch, and a new epoch discards the whole cache.
//!
//! The screen itself is re-read on every save, together with the part of a
//! logical line that starts in history and continues onto it.
//!
//! Chunks join exactly as one whole read would format them. A chunk is one of
//! two kinds:
//!
//! - A closed chunk starts on a logical line (or continues an open chunk) and
//!   ends on a row that ends a logical line and has visible text, so the
//!   formatter's trailing-blank-line trim removes nothing from it, and the
//!   formatter closes all SGR and hyperlink state at every line end. It is
//!   followed by the line separator.
//! - An open chunk ends on a soft-wrapped row, in the middle of a logical
//!   line. It is what lets a line longer than a chunk be formatted a chunk at
//!   a time instead of under one lock hold: the formatter emits every cell of
//!   the last row, closes nothing and trims nothing, and the chunk keeps the
//!   SGR and hyperlink state it stopped in ([`shepr_vt::AnsiCarry`]). The
//!   chunk after it starts from that state, so the style changes it emits are
//!   the ones the whole line has at that point, and the two join with no
//!   separator. The whole line's bytes are the pieces' bytes back to back.
//!
//! The whole read is the chunks and the screen read joined this way. Rows in
//! history never change, so the state an open chunk stops in (the style of
//! its last cell) is a function of its last row alone, which is why a chunk
//! re-read after its first rows were evicted still hands the next chunk the
//! state that chunk was formatted from. The exception is a chunk that
//! starts from a carried state and whose predecessor was evicted whole: its
//! text was formatted from a state the whole read no longer has, so it is
//! dropped and its rows are read again.
//!
//! Rows are only cut where the join is exact: at a line end with visible
//! text, or at a soft wrap. A run of rows that is neither (thousands of
//! blank lines in a row) is still formatted under one hold.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use shepr_vt::AnsiCarry;

use super::*;

/// Chunks of formatted history a reader keeps between reads of one pane,
/// oldest first and contiguous. Only a reader of the same terminal may use
/// it: a read through another terminal's source starts it afresh.
///
/// The cache also keeps the screen part of the last successful read, so the
/// pane's last primary history can be rebuilt (`pieces`) while the alternate
/// screen hides it, and a revision that changes exactly when that text does,
/// so a save can tell that nothing changed without comparing text.
#[derive(Default)]
pub struct PaneHistoryCache {
    terminal: Weak<PaneTerminal>,
    epoch: u64,
    cols: u16,
    chunks: VecDeque<HistoryChunk>,
    /// The screen part of the last successful read.
    tail: Arc<str>,
    /// Names the current text: taken from `next_revision` whenever a chunk or
    /// the tail changes. Zero while the cache has never held text.
    revision: u64,
}

/// Revisions are unique across every cache, so a replaced or recreated cache
/// never repeats one a save has already seen.
fn next_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The VT text of absolute rows `start..end`: whole logical lines, the last
/// of which has visible text, or (`open`) rows that stop inside a logical
/// line.
struct HistoryChunk {
    start: AbsRow,
    end: AbsRow,
    text: Arc<str>,
    /// The text was formatted from a state carried over from the chunk before
    /// it, which ended inside the logical line this one continues.
    resumed: bool,
    /// Set when the chunk ends inside a logical line: the state the chunk
    /// after it starts from.
    open: Option<AnsiCarry>,
}

/// One piece of a pane's history text, shared with the cache it came from.
/// The text of a pane is its pieces in order, each joined to the one before
/// it by a line break or by nothing.
#[derive(Clone, Debug)]
pub struct HistoryPiece {
    pub text: Arc<str>,
    /// Whether `\r\n` separates this piece from the one before it. False for
    /// the first piece and for a piece that continues a logical line.
    pub break_before: bool,
}

/// A pane terminal's history, readable from any thread. Cheap to take on the
/// event loop: it holds the terminal, and reading it happens wherever the
/// reader runs.
#[derive(Clone)]
pub struct PaneHistorySource(pub(crate) Arc<PaneTerminal>);

impl PaneHistorySource {
    /// Brings `cache` up to date with the pane's primary-screen history,
    /// formatting only what it does not already hold. `false`, with the
    /// cache left as it was, while the alternate screen is active (the
    /// inactive primary grid cannot be read, and a full-screen program's
    /// frame is not history) or when the terminal cannot be read.
    pub fn refresh(&self, cache: &mut PaneHistoryCache) -> bool {
        if !std::ptr::eq(Weak::as_ptr(&cache.terminal), Arc::as_ptr(&self.0)) {
            *cache = PaneHistoryCache {
                terminal: Arc::downgrade(&self.0),
                ..PaneHistoryCache::default()
            };
        }
        self.0.read_primary_history(cache).is_some()
    }
}

/// Where the retained primary screen stands under one lock hold.
struct HistoryBounds {
    /// The oldest retained row.
    origin: AbsRow,
    /// The first screen row: rows before it are history and do not change.
    screen_start: AbsRow,
    cols: u16,
}

impl HistoryBounds {
    fn of(terminal: &shepr_vt::Terminal) -> Self {
        let total_rows = terminal.total_rows();
        let screen_rows = usize::from(terminal.rows());
        Self {
            origin: terminal.history_origin(),
            screen_start: terminal
                .absolute_row_for_screen(ScreenRow(total_rows.saturating_sub(screen_rows))),
            cols: terminal.cols(),
        }
    }
}

impl PaneHistoryCache {
    /// Drops what the terminal no longer backs: everything after a resize,
    /// evicted rows at the front, rows that are on the screen again at the
    /// back. Returns whether anything was dropped.
    fn settle(&mut self, epoch: u64, bounds: &HistoryBounds) -> bool {
        let mut changed = false;
        if self.epoch != epoch || self.cols != bounds.cols {
            changed = !self.chunks.is_empty();
            self.chunks.clear();
            self.epoch = epoch;
            self.cols = bounds.cols;
        }
        while self
            .chunks
            .front()
            .is_some_and(|chunk| chunk.start < bounds.origin)
        {
            self.chunks.pop_front();
            changed = true;
        }
        // A chunk that continues a line whose earlier rows are gone was
        // formatted from a state the whole read no longer has.
        while self
            .chunks
            .front()
            .is_some_and(|chunk| chunk.resumed && chunk.start <= bounds.origin)
        {
            self.chunks.pop_front();
            changed = true;
        }
        while self
            .chunks
            .back()
            .is_some_and(|chunk| chunk.end > bounds.screen_start)
        {
            self.chunks.pop_back();
            changed = true;
        }
        if changed {
            self.revision = next_revision();
        }
        changed
    }

    /// The first row the cache does not cover, if it covers any.
    fn end(&self) -> Option<AbsRow> {
        self.chunks.back().map(|chunk| chunk.end)
    }

    /// The state the read of the row at `end` starts from: the state the last
    /// chunk stopped in, or the default when it ended a logical line.
    fn open_carry(&self) -> AnsiCarry {
        self.chunks
            .back()
            .and_then(|chunk| chunk.open.clone())
            .unwrap_or_default()
    }

    fn push_front(&mut self, chunk: HistoryChunk) {
        self.chunks.push_front(chunk);
        self.revision = next_revision();
    }

    fn push_back(&mut self, chunk: HistoryChunk) {
        self.chunks.push_back(chunk);
        self.revision = next_revision();
    }

    /// Forgets all text, the tail included.
    fn clear(&mut self) {
        if !self.chunks.is_empty() || !self.tail.is_empty() {
            self.chunks.clear();
            self.tail = Arc::default();
            self.revision = next_revision();
        }
    }

    /// Records the screen part of a read.
    fn set_tail(&mut self, tail: String) {
        if *self.tail != *tail {
            self.tail = tail.into();
            self.revision = next_revision();
        }
    }

    /// Whether the cache holds any visible text. Every chunk has visible
    /// text, so only the tail needs looking at when there are none.
    pub fn has_text(&self) -> bool {
        !self.chunks.is_empty() || !self.tail.trim().is_empty()
    }

    /// Names the text this cache holds: it changes whenever the text does and
    /// is never shared with another cache.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The cached chunks followed by the tail of the last read, as pieces
    /// that share the cache's text: the pane's primary history as of the last
    /// successful read, even while the alternate screen hides it now. Joined
    /// as one read would join their lines.
    pub fn pieces(&self) -> Vec<HistoryPiece> {
        let mut pieces = Vec::with_capacity(self.chunks.len() + 1);
        // Whether the piece before the next one ended a logical line.
        let mut line_ended = true;
        for chunk in &self.chunks {
            pieces.push(HistoryPiece {
                text: Arc::clone(&chunk.text),
                break_before: !pieces.is_empty() && line_ended,
            });
            line_ended = chunk.open.is_none();
        }
        if !self.tail.is_empty() {
            pieces.push(HistoryPiece {
                text: Arc::clone(&self.tail),
                break_before: !pieces.is_empty() && line_ended,
            });
        }
        pieces
    }

    /// [`pieces`] as one string.
    ///
    /// [`pieces`]: Self::pieces
    pub fn text(&self) -> String {
        let mut text = String::new();
        for piece in self.pieces() {
            if piece.break_before {
                text.push_str("\r\n");
            }
            text.push_str(&piece.text);
        }
        text
    }
}

/// The end of the last chunk that may end inside `from..to`: one past the
/// latest row there that ends a logical line and has visible text. Rows are
/// checked from the end, so the search usually stops at the first row.
fn chunk_boundary(terminal: &shepr_vt::Terminal, from: AbsRow, to: AbsRow) -> Option<AbsRow> {
    let mut scratch = String::new();
    let mut row = to;
    while row > from {
        row = row.saturating_sub(1);
        let y = terminal.screen_row_for_absolute(row)?;
        let mut visible = false;
        let wrap = terminal.visit_screen_row_text(y, &mut scratch, |_, _, text| {
            visible = visible || !text.trim().is_empty();
        })?;
        if visible && !wrap.soft_wrapped {
            return Some(row.saturating_add(1));
        }
    }
    None
}

/// The end of the last open chunk that may end inside `from..to`: one past
/// the latest soft-wrapped row there, whose logical line continues in the
/// next row. Rows are checked from the end, so inside a long line the search
/// stops at the first row.
fn wrap_boundary(terminal: &shepr_vt::Terminal, from: AbsRow, to: AbsRow) -> Option<AbsRow> {
    let mut row = to;
    while row > from {
        row = row.saturating_sub(1);
        let y = terminal.screen_row_for_absolute(row)?;
        if terminal.screen_row_wrap(y)?.soft_wrapped {
            return Some(row.saturating_add(1));
        }
    }
    None
}

/// VT text of absolute rows `start..end`, unwrapped, read from the state
/// `carry` holds. With `open_end` the rows stop inside a logical line (their
/// last row is soft-wrapped) and the second value is the state the next rows
/// start from; otherwise trailing blank lines are trimmed and it is `None`.
fn format_chunk(
    terminal: &shepr_vt::Terminal,
    start: AbsRow,
    end: AbsRow,
    carry: &AnsiCarry,
    open_end: bool,
) -> Option<(String, Option<AnsiCarry>)> {
    let cols = terminal.cols();
    let first = terminal.screen_row_for_absolute(start)?;
    let last = terminal.screen_row_for_absolute(end.saturating_sub(1))?;
    let mut carry = carry.clone();
    let text = terminal
        .read_ansi_screen_carrying(
            Point::new(first, 0),
            Point::new(last, cols.saturating_sub(1)),
            &mut carry,
            open_end,
        )
        .ok()?;
    Some((text, open_end.then_some(carry)))
}

impl PaneTerminal {
    /// See [`PaneHistorySource::refresh`].
    pub(crate) fn read_primary_history(&self, cache: &mut PaneHistoryCache) -> Option<()> {
        // Where the search for the next chunk boundary resumes: rows between
        // the cache's end and here were searched without finding one.
        let mut probe: Option<AbsRow> = None;
        loop {
            let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
            let terminal = &core.terminal;
            if terminal.active_screen() != shepr_vt::ActiveScreen::Primary {
                return None;
            }
            let bounds = HistoryBounds::of(terminal);
            if cache.settle(core.history_epoch, &bounds) {
                probe = None;
            }

            // Rows evicted from the middle of the oldest chunk took that
            // chunk with them; its surviving rows come back as a chunk of
            // their own, which ends where the next chunk starts and, like
            // the chunk it replaces, stops inside a logical line exactly when
            // the next chunk continues one.
            if let Some(front) = cache.chunks.front()
                && front.start > bounds.origin
            {
                let end = front.start;
                let open_end = front.resumed;
                let Some((text, open)) = format_chunk(
                    terminal,
                    bounds.origin,
                    end,
                    &AnsiCarry::default(),
                    open_end,
                ) else {
                    cache.clear();
                    return None;
                };
                cache.push_front(HistoryChunk {
                    start: bounds.origin,
                    end,
                    text: text.into(),
                    resumed: false,
                    open,
                });
                drop(core);
                std::thread::yield_now();
                continue;
            }

            let next = cache.end().unwrap_or(bounds.origin);
            let from = probe.filter(|probe| *probe > next).unwrap_or(next);
            if from < bounds.screen_start {
                let window_end = bounds
                    .screen_start
                    .min(from.saturating_add(SCAN_CHUNK_ROWS));
                let carry = cache.open_carry();
                let resumed = !carry.is_fresh();
                let chunk = match chunk_boundary(terminal, from, window_end) {
                    Some(end) => Some((end, false)),
                    // A whole chunk's worth of rows without a line end to cut
                    // at: cut inside the line, at the last soft wrap.
                    None if window_end.0.saturating_sub(next.0) >= SCAN_CHUNK_ROWS => {
                        wrap_boundary(terminal, from, window_end).map(|end| (end, true))
                    }
                    None => None,
                };
                match chunk {
                    Some((end, open_end)) => {
                        let Some((text, open)) =
                            format_chunk(terminal, next, end, &carry, open_end)
                        else {
                            cache.clear();
                            return None;
                        };
                        cache.push_back(HistoryChunk {
                            start: next,
                            end,
                            text: text.into(),
                            resumed,
                            open,
                        });
                        probe = None;
                    }
                    None => probe = Some(window_end),
                }
                drop(core);
                // Give the PTY reader waiting on the lock a chance to take it.
                std::thread::yield_now();
                continue;
            }

            // The cache reaches the last row it can end a chunk at; the rest
            // is read now, under this hold, up to the last row with content
            // or the cursor.
            let Ok(range) = terminal_recent_read_range(terminal, usize::MAX) else {
                cache.clear();
                return None;
            };
            let Some((_, end, _)) = range else {
                // Nothing to read at all: the read is empty, whatever the
                // cache held.
                cache.clear();
                return Some(());
            };
            let carry = cache.open_carry();
            let tail = match terminal.screen_row_for_absolute(next) {
                Some(start) if start.0 <= end => {
                    let last = terminal
                        .absolute_row_for_screen(ScreenRow(end))
                        .saturating_add(1);
                    let Some((tail, _)) = format_chunk(terminal, next, last, &carry, false) else {
                        // Chunks may have advanced past the old tail.
                        cache.clear();
                        return None;
                    };
                    tail
                }
                // A line the last chunk left open has no rows left to end it.
                _ if !carry.is_fresh() => {
                    cache.clear();
                    return None;
                }
                _ => String::new(),
            };
            drop(core);
            cache.set_tail(tail);
            return Some(());
        }
    }
}

#[cfg(test)]
impl PaneHistorySource {
    /// The pane's primary-screen history as VT text after a [`refresh`]
    /// (`None` when that failed).
    ///
    /// [`refresh`]: Self::refresh
    pub fn read(&self, cache: &mut PaneHistoryCache) -> Option<String> {
        self.refresh(cache).then(|| cache.text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(cols: u16, rows: u16, scrollback_bytes: usize) -> Arc<PaneTerminal> {
        Arc::new(PaneTerminal::new(shepr_vt::Terminal::new(
            cols,
            rows,
            scrollback_bytes,
        )))
    }

    fn write(terminal: &PaneTerminal, bytes: &[u8]) {
        shepr_vt::lock_terminal_core(&terminal.core)
            .expect("test precondition")
            .terminal
            .write(bytes);
    }

    /// The history as one read of every retained row formats it, under a
    /// single lock hold.
    fn whole_read(terminal: &Arc<PaneTerminal>) -> Option<String> {
        let mut core = shepr_vt::lock_terminal_core(&terminal.core).expect("test precondition");
        if core.terminal.active_screen() != shepr_vt::ActiveScreen::Primary {
            return None;
        }
        terminal_recent_ansi_snapshot(&mut core, usize::MAX, true)
            .ok()
            .map(|snapshot| snapshot.text)
    }

    #[test]
    fn cached_reads_match_a_whole_read_as_output_scrolls_and_evicts() {
        // The smallest history keeps a thousand lines; the rounds below
        // write several times that.
        let pane = terminal(12, 4, 2048);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        for round in 0..300 {
            let mut output = String::new();
            for line in 0..7 {
                // Blank lines, styled text and lines that wrap all take part
                // in chunk boundaries.
                match line % 4 {
                    0 => output.push_str("\r\n"),
                    1 => output.push_str(&format!("\x1b[1;31mred {round}-{line}\x1b[0m\r\n")),
                    2 => output.push_str(&format!("a long line that wraps {round}-{line}\r\n")),
                    _ => output.push_str(&format!("plain {round}-{line}\r\n")),
                }
            }
            write(&pane, output.as_bytes());
            let cached = source.read(&mut cache);
            assert_eq!(cached, whole_read(&pane), "round {round}");
        }
        assert!(
            shepr_vt::lock_terminal_core(&pane.core)
                .expect("test precondition")
                .terminal
                .history_origin()
                > AbsRow(0),
            "the test must evict history to cover eviction"
        );
    }

    #[test]
    fn a_resize_starts_the_cache_over() {
        let pane = terminal(20, 4, 4096);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        for line in 0..20 {
            write(&pane, format!("line {line}\r\n").as_bytes());
        }
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        // A taller pane pulls history rows back onto the screen, where the
        // child rewrites one before the pane shrinks again.
        let _ = pane.resize(shepr_core::geometry::PaneGeometry::new(20, 10, 0, 0));
        write(&pane, b"\x1b[Hrewritten\r\n");
        let _ = pane.resize(shepr_core::geometry::PaneGeometry::new(20, 4, 0, 0));
        let read = source.read(&mut cache);
        assert_eq!(read, whole_read(&pane));
    }

    #[test]
    fn the_alternate_screen_hides_history_and_keeps_the_cache() {
        let pane = terminal(20, 4, 4096);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        for line in 0..20 {
            write(&pane, format!("line {line}\r\n").as_bytes());
        }
        let before = source.read(&mut cache);
        assert!(
            before
                .as_deref()
                .is_some_and(|text| text.contains("line 0"))
        );
        write(&pane, b"\x1b[?1049hFULL SCREEN");
        assert_eq!(source.read(&mut cache), None);
        assert!(
            !cache.chunks.is_empty(),
            "the cache outlives the full-screen program"
        );
        write(&pane, b"\x1b[?1049l");
        let after = source.read(&mut cache);
        assert_eq!(after, whole_read(&pane));
        assert!(after.is_some_and(|text| text.contains("line 0") && !text.contains("FULL")));
    }

    #[test]
    fn a_cache_from_another_terminal_is_not_trusted() {
        let first = terminal(20, 4, 4096);
        let second = terminal(20, 4, 4096);
        for line in 0..20 {
            write(&first, format!("first {line}\r\n").as_bytes());
            write(&second, format!("second {line}\r\n").as_bytes());
        }
        let mut cache = PaneHistoryCache::default();
        PaneHistorySource(Arc::clone(&first)).read(&mut cache);
        let read = PaneHistorySource(Arc::clone(&second)).read(&mut cache);
        assert_eq!(read, whole_read(&second));
        assert!(read.is_some_and(|text| !text.contains("first")));
    }

    /// One logical line of `chars` characters that changes style every few
    /// cells, opens and closes hyperlinks across row ends, and has runs of
    /// blanks that land on row ends, so cuts fall in the middle of every kind
    /// of state. Ends with a styled run of blanks.
    fn long_styled_line(chars: usize) -> String {
        const STYLES: [&str; 5] = [
            "\x1b[0m",
            "\x1b[1;31m",
            "\x1b[4;38;5;99m",
            "\x1b[7m",
            "\x1b[0;42m",
        ];
        let mut line = String::new();
        for i in 0..chars {
            if i % 7 == 0 {
                line.push_str(STYLES[(i / 7) % STYLES.len()]);
            }
            if i % 90 == 0 {
                line.push_str(&format!("\x1b]8;;https://example.com/{i}\x1b\\"));
            }
            if i % 90 == 40 {
                line.push_str("\x1b]8;;\x1b\\");
            }
            line.push(char::from(b'a' + u8::try_from(i % 26).unwrap_or(0)));
            if i % 31 == 30 {
                line.push_str("  ");
            }
        }
        line.push_str("\x1b[1;31m");
        line.push_str(&" ".repeat(30));
        line
    }

    /// Room for several thousand rows of twelve columns, more than a chunk.
    const DEEP_HISTORY_BYTES: usize = 1_200_000;

    #[test]
    fn a_line_longer_than_a_chunk_is_read_in_pieces_that_join_exactly() {
        let pane = terminal(12, 4, DEEP_HISTORY_BYTES);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        write(&pane, b"before\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));

        // The line grows across saves, so the cache meets it while it still
        // continues onto the screen, and again once it is far in history.
        let line = long_styled_line(40_000);
        let mut written = 0;
        while written < line.len() {
            let end = (written + 6_001).min(line.len());
            write(&pane, &line.as_bytes()[written..end]);
            written = end;
            assert_eq!(source.read(&mut cache), whole_read(&pane), "at {written}");
        }
        write(&pane, b"\r\nafter\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        for round in 0..10 {
            write(&pane, format!("line {round}\r\n").as_bytes());
            assert_eq!(source.read(&mut cache), whole_read(&pane), "round {round}");
        }

        assert!(
            cache.chunks.iter().any(|chunk| chunk.open.is_some()),
            "the long line is cached in open pieces"
        );
        assert!(
            cache
                .chunks
                .iter()
                .all(|chunk| chunk.end.0 - chunk.start.0 <= SCAN_CHUNK_ROWS),
            "no chunk is longer than a window"
        );
        assert!(cache.chunks.len() > 2);
        // Pieces share the cache's text and join to the whole read.
        let pieces = cache.pieces();
        assert!(pieces.iter().skip(1).any(|piece| !piece.break_before));
        assert_eq!(Some(cache.text()), whole_read(&pane));
    }

    #[test]
    fn eviction_through_the_pieces_of_a_long_line_keeps_reads_exact() {
        let pane = terminal(12, 4, DEEP_HISTORY_BYTES);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        write(&pane, long_styled_line(30_000).as_bytes());
        write(&pane, b"\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));

        // Enough further lines to push the whole line out of a history of a
        // few thousand rows, read every hundred so the origin passes through
        // the inside of every piece.
        let mut origins = Vec::new();
        for round in 0..60 {
            let mut output = String::new();
            for line in 0..100 {
                output.push_str(&format!("row {round}-{line}\r\n"));
            }
            write(&pane, output.as_bytes());
            assert_eq!(source.read(&mut cache), whole_read(&pane), "round {round}");
            origins.push(
                shepr_vt::lock_terminal_core(&pane.core)
                    .expect("test precondition")
                    .terminal
                    .history_origin()
                    .0,
            );
        }
        assert!(
            origins
                .iter()
                .any(|origin| *origin > 0 && *origin < SCAN_CHUNK_ROWS),
            "the origin passed through the first piece: {origins:?}"
        );
        assert!(origins.last().is_some_and(|origin| *origin > 2_500));
    }
}
