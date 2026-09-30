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
//! Chunks join exactly as one whole read would format them. A chunk is the
//! rows from where the cache ends to the end of the scan window
//! (`SCAN_CHUNK_ROWS` rows at most, or the screen start), whatever those rows
//! hold, and is one of two kinds:
//!
//! - A closed chunk ends on a row that ends a logical line. The formatter
//!   closes all SGR and hyperlink state at every line end, so the chunk starts
//!   and ends in the default state, and the next chunk follows it after a
//!   line separator.
//! - An open chunk ends on a soft-wrapped row, in the middle of a logical
//!   line. The formatter emits every cell of the last row, closes nothing and
//!   trims nothing, and the chunk keeps the SGR and hyperlink state it
//!   stopped in ([`shepr_vt::AnsiCarry`]). The chunk after it starts from that
//!   state, so the style changes it emits are the ones the whole line has at
//!   that point, and the two join with no separator. The whole line's bytes
//!   are the pieces' bytes back to back. This is what lets a line longer than
//!   a chunk be formatted a chunk at a time.
//!
//! A chunk is not cut back to its content. The formatter reports, beside its
//! untruncated text, the byte length the text has when trailing blank lines
//! are dropped (`content_end`): `None` when the chunk has no content (blank
//! is what the formatter would trim: painted, underlined, inverse,
//! struck-through or hyperlinked blank cells are content in a VT read),
//! `Some(0)` when it only finishes a logical line that began in an earlier
//! chunk. Only the whole read is cut back, and where depends on rows the
//! reader has not formatted yet, so the cache keeps every chunk and the
//! screen part ("tail") as formatted, blank runs included, with their
//! content ends. Exposure ([`PaneHistoryCache::pieces`], `text` and
//! `has_text`) is the whole read: the pieces up to the last one that has
//! content, the last one cut at its content end, everything after it, and the
//! separator in front of it, dropped. A blank chunk in front of later
//! content is part of the text, and a cache may hold nothing but blank chunks
//! (its exposed text is then empty).
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
//! Since chunks are cut at the window end and nowhere else, no run of rows,
//! however blank, is formatted under one lock hold longer than a window.
//!
//! Saves that find a few new rows each would leave a chunk per save, so a
//! chunk just added is merged into the one before it (after the lock is
//! released) while the result stays within `MERGE_MAX_ROWS` rows and
//! `MERGE_MAX_BYTES` bytes. The merged text is the two texts with the
//! separator the first one owes (none when it is open), so it is the bytes a
//! whole read has for those rows; it keeps the first chunk's start and
//! `resumed` and the second one's end and `open`, and its content end is the
//! second's shifted past the first (or the first's when the second has none).
//! A merged chunk is dropped whole by eviction like any other and its
//! surviving rows are formatted again, which the row cap keeps small. The
//! exposed text does not change, so neither does the revision.

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
/// screen hides it, and a revision that changes whenever that text does, so a
/// save can tell that nothing changed without comparing text.
#[derive(Default)]
pub struct PaneHistoryCache {
    terminal: Weak<PaneTerminal>,
    epoch: u64,
    cols: u16,
    chunks: VecDeque<HistoryChunk>,
    /// The screen part of the last successful read, untrimmed.
    tail: Arc<str>,
    /// The byte length of `tail` cut back to its content, as for a chunk.
    tail_content_end: Option<usize>,
    /// Names the current text: taken from `next_revision` whenever a chunk or
    /// the tail changes. Equal revisions mean the exposed text is unchanged;
    /// different ones do not mean it changed (a change past the last content,
    /// such as one more blank line, moves the revision and not the text).
    /// Zero while the cache has never held anything.
    revision: u64,
}

/// Revisions are unique across every cache, so a replaced or recreated cache
/// never repeats one a save has already seen.
fn next_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The VT text of absolute rows `start..end`, as the formatter wrote it:
/// trailing blank lines included. Whole logical lines, or (`open`) rows that
/// stop inside a logical line.
#[derive(Clone)]
struct HistoryChunk {
    start: AbsRow,
    end: AbsRow,
    text: Arc<str>,
    /// The byte length of `text` cut back to its content: `None` when the
    /// chunk has none, `Some(0)` when it only finishes a line that began in
    /// the chunk before it, the whole length when it is open.
    content_end: Option<usize>,
    /// The text was formatted from a state carried over from the chunk before
    /// it, which ended inside the logical line this one continues.
    resumed: bool,
    /// Set when the chunk ends inside a logical line: the state the chunk
    /// after it starts from.
    open: Option<AnsiCarry>,
}

impl HistoryChunk {
    fn rows(&self) -> u64 {
        self.end.0.saturating_sub(self.start.0)
    }

    /// The line separator between this chunk's text and the next chunk's:
    /// present exactly when this chunk ends a logical line.
    fn separator(&self) -> &'static str {
        if self.open.is_some() { "" } else { "\r\n" }
    }

    /// Whether `next`, the chunk right after this one, can be joined into it.
    /// Any two adjacent chunks can be as far as exactness goes; only the size
    /// caps decide.
    fn can_absorb(&self, next: &Self) -> bool {
        self.rows().saturating_add(next.rows()) <= MERGE_MAX_ROWS
            && self
                .text
                .len()
                .saturating_add(self.separator().len())
                .saturating_add(next.text.len())
                <= MERGE_MAX_BYTES
    }

    /// Joins `next` onto this chunk as a whole read joins them: the text of
    /// this one, the separator it owes the next (none when it is open) and the
    /// text of the next. The result starts from this chunk's state
    /// (`resumed`) and stops in the next one's (`open`).
    ///
    /// The content end is where the whole read would cut the joined text: at
    /// the next chunk's own content end, shifted past this text and the
    /// separator (`Some(0)` lands right after them, as `pieces` cuts such a
    /// piece), and when the next has no content, at this chunk's, since
    /// everything after that is trailing blank.
    fn absorb(&mut self, next: Self) {
        let separator = self.separator();
        let offset = self.text.len() + separator.len();
        let mut text = String::with_capacity(offset + next.text.len());
        text.push_str(&self.text);
        text.push_str(separator);
        text.push_str(&next.text);
        self.text = text.into();
        self.content_end = match next.content_end {
            Some(end) => Some(offset + end),
            None => self.content_end,
        };
        self.end = next.end;
        self.open = next.open;
    }
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
        let mut updated = if std::ptr::eq(Weak::as_ptr(&cache.terminal), Arc::as_ptr(&self.0)) {
            cache.duplicate()
        } else {
            PaneHistoryCache {
                terminal: Arc::downgrade(&self.0),
                ..PaneHistoryCache::default()
            }
        };
        if self.0.read_primary_history_inner(&mut updated).is_some() {
            *cache = updated;
            true
        } else {
            false
        }
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
    fn duplicate(&self) -> Self {
        Self {
            terminal: Weak::clone(&self.terminal),
            epoch: self.epoch,
            cols: self.cols,
            chunks: self.chunks.clone(),
            tail: Arc::clone(&self.tail),
            tail_content_end: self.tail_content_end,
            revision: self.revision,
        }
    }

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

    /// Merges the chunk at `index` into the one before it when
    /// [`HistoryChunk::can_absorb`] allows. Copies at most the merge byte cap,
    /// so a reader calls it after it has let go of the terminal lock. The
    /// exposed text does not change, so the revision does not either (the push
    /// that made the chunk already moved it).
    fn coalesce_at(&mut self, index: usize) {
        let Some(before) = index.checked_sub(1) else {
            return;
        };
        let mergeable = match (self.chunks.get(before), self.chunks.get(index)) {
            (Some(first), Some(second)) => first.can_absorb(second),
            _ => false,
        };
        if !mergeable {
            return;
        }
        let Some(second) = self.chunks.remove(index) else {
            return;
        };
        if let Some(first) = self.chunks.get_mut(before) {
            first.absorb(second);
        }
    }

    /// Forgets all text, the tail included.
    fn clear(&mut self) {
        if !self.chunks.is_empty() || !self.tail.is_empty() {
            self.chunks.clear();
            self.tail = Arc::default();
            self.tail_content_end = None;
            self.revision = next_revision();
        }
    }

    /// Records the screen part of a read.
    fn set_tail(&mut self, tail: String, content_end: Option<usize>) {
        if *self.tail != *tail || self.tail_content_end != content_end {
            self.tail = tail.into();
            self.tail_content_end = content_end;
            self.revision = next_revision();
        }
    }

    /// What the whole read is made of, oldest first: the chunks, then the
    /// tail. Each is its text as formatted, its content end and whether it
    /// stops inside a logical line.
    fn parts(&self) -> impl Iterator<Item = (&Arc<str>, Option<usize>, bool)> {
        self.chunks
            .iter()
            .map(|chunk| (&chunk.text, chunk.content_end, chunk.open.is_some()))
            .chain(std::iter::once((&self.tail, self.tail_content_end, false)))
    }

    /// The index in [`parts`] of the last part that has content, where the
    /// whole read ends.
    ///
    /// [`parts`]: Self::parts
    fn last_content(&self) -> Option<usize> {
        if self.tail_content_end.is_some() {
            Some(self.chunks.len())
        } else {
            self.chunks
                .iter()
                .rposition(|chunk| chunk.content_end.is_some())
        }
    }

    /// Whether the exposed text has anything but whitespace in it.
    pub fn has_text(&self) -> bool {
        let Some(last) = self.last_content() else {
            return false;
        };
        self.parts()
            .take(last + 1)
            .enumerate()
            .any(|(index, (text, content_end, _))| {
                let full: &str = text;
                let exposed = match content_end {
                    Some(end) if index == last => full.get(..end).unwrap_or(full),
                    _ => full,
                };
                !exposed.trim().is_empty()
            })
    }

    /// Names the exposed text: equal revisions mean the same text. It is never
    /// shared with another cache. A different revision does not prove the text
    /// changed: blank lines past the last content move it without changing
    /// the text.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The whole read as pieces that share the cache's text: the pane's
    /// primary history as of the last successful read, even while the
    /// alternate screen hides it now. Joined as one read would join their
    /// lines. The read ends at its last content: the parts after the last one
    /// that has content, and the line separator in front of them, are left
    /// out, and that part is cut back to its content end. A blank part in
    /// front of later content stays (an empty piece is a blank line).
    pub fn pieces(&self) -> Vec<HistoryPiece> {
        let Some(last) = self.last_content() else {
            return Vec::new();
        };
        let mut pieces = Vec::with_capacity(last + 1);
        // Whether the piece before the next one ended a logical line.
        let mut line_ended = true;
        for (index, (text, content_end, open)) in self.parts().take(last + 1).enumerate() {
            let full: &str = text;
            let text = match content_end {
                Some(end) if index == last && end < full.len() => {
                    Arc::from(full.get(..end).unwrap_or(full))
                }
                _ => Arc::clone(text),
            };
            pieces.push(HistoryPiece {
                text,
                break_before: !pieces.is_empty() && line_ended,
            });
            line_ended = !open;
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

/// Absolute rows `start..end` as the formatter wrote them.
struct Formatted {
    /// Unwrapped VT text, trailing blank lines included.
    text: String,
    /// The byte length of `text` cut back to its content (see
    /// [`HistoryChunk::content_end`]).
    content_end: Option<usize>,
    /// With `open_end`, the state the next rows start from.
    open: Option<AnsiCarry>,
}

/// VT text of absolute rows `start..end`, unwrapped, read from the state
/// `carry` holds. With `open_end` the rows stop inside a logical line (their
/// last row is soft-wrapped) and `open` is the state the next rows start
/// from. The text is not cut back to its content.
fn format_chunk(
    terminal: &shepr_vt::Terminal,
    start: AbsRow,
    end: AbsRow,
    carry: &AnsiCarry,
    open_end: bool,
) -> Option<Formatted> {
    let cols = terminal.cols();
    let first = terminal.screen_row_for_absolute(start)?;
    let last = terminal.screen_row_for_absolute(end.saturating_sub(1))?;
    let mut carry = carry.clone();
    let (text, content_end) = terminal
        .read_ansi_screen_carrying(
            Point::new(first, 0),
            Point::new(last, cols.saturating_sub(1)),
            &mut carry,
            open_end,
        )
        .ok()?;
    Some(Formatted {
        text,
        content_end,
        open: open_end.then_some(carry),
    })
}

impl PaneTerminal {
    /// See [`PaneHistorySource::refresh`].
    pub(crate) fn read_primary_history(&self, cache: &mut PaneHistoryCache) -> Option<()> {
        let mut updated = cache.duplicate();
        self.read_primary_history_inner(&mut updated)?;
        *cache = updated;
        Some(())
    }

    fn read_primary_history_inner(&self, cache: &mut PaneHistoryCache) -> Option<()> {
        loop {
            let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
            let terminal = &core.terminal;
            if terminal.active_screen() != shepr_vt::ActiveScreen::Primary {
                return None;
            }
            let bounds = HistoryBounds::of(terminal);
            cache.settle(core.history_epoch, &bounds);

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
                let formatted = format_chunk(
                    terminal,
                    bounds.origin,
                    end,
                    &AnsiCarry::default(),
                    open_end,
                )?;
                cache.push_front(HistoryChunk {
                    start: bounds.origin,
                    end,
                    text: formatted.text.into(),
                    content_end: formatted.content_end,
                    resumed: false,
                    open: formatted.open,
                });
                drop(core);
                cache.coalesce_at(1);
                std::thread::yield_now();
                continue;
            }

            let next = cache.end().unwrap_or(bounds.origin);
            if next < bounds.screen_start {
                // The chunk is the whole window, cut wherever it ends: open
                // when its last row is soft-wrapped, closed otherwise.
                let end = bounds
                    .screen_start
                    .min(next.saturating_add(SCAN_CHUNK_ROWS));
                let open_end = terminal
                    .screen_row_for_absolute(end.saturating_sub(1))
                    .and_then(|y| terminal.screen_row_wrap(y))
                    .map(|wrap| wrap.soft_wrapped)?;
                let carry = cache.open_carry();
                let resumed = !carry.is_fresh();
                let formatted = format_chunk(terminal, next, end, &carry, open_end)?;
                cache.push_back(HistoryChunk {
                    start: next,
                    end,
                    text: formatted.text.into(),
                    content_end: formatted.content_end,
                    resumed,
                    open: formatted.open,
                });
                drop(core);
                // Merged with the lock released: it is a copy of cached text.
                cache.coalesce_at(cache.chunks.len().saturating_sub(1));
                // Give the PTY reader waiting on the lock a chance to take it.
                std::thread::yield_now();
                continue;
            }

            // The cache reaches the screen; the screen is read now, under
            // this hold, up to the last row with content or the cursor.
            let Ok(range) = terminal_recent_read_range(terminal, usize::MAX) else {
                return None;
            };
            let Some((_, end, _)) = range else {
                // Nothing to read at all: the read is empty, whatever the
                // cache held.
                cache.clear();
                drop(core);
                return Some(());
            };
            let carry = cache.open_carry();
            let (tail, content_end) = match terminal.screen_row_for_absolute(next) {
                Some(start) if start.0 <= end => {
                    let last = terminal
                        .absolute_row_for_screen(ScreenRow(end))
                        .saturating_add(1);
                    let Some(formatted) = format_chunk(terminal, next, last, &carry, false) else {
                        // The caller discards this partial refresh, including
                        // any chunks settled or formatted earlier in the read.
                        return None;
                    };
                    (formatted.text, formatted.content_end)
                }
                // A line the last chunk left open has no rows left to end it.
                _ if !carry.is_fresh() => {
                    return None;
                }
                _ => (String::new(), None),
            };
            drop(core);
            cache.set_tail(tail, content_end);
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
        terminal_recent_ansi_snapshot(&mut core, usize::MAX)
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

    /// Room for well over ten thousand rows of twelve columns, so runs of
    /// several windows of blank rows stay retained.
    const VERY_DEEP_HISTORY_BYTES: usize = 6_000_000;

    /// More blank rows than one scan window holds.
    const BLANK_RUN: usize = 3_000;

    fn write_blank_lines(pane: &PaneTerminal, count: usize) {
        write(pane, "\r\n".repeat(count).as_bytes());
    }

    /// Whether some chunk of the cache has no content.
    fn holds_blank_chunk(cache: &PaneHistoryCache) -> bool {
        cache.chunks.iter().any(|chunk| chunk.content_end.is_none())
    }

    #[test]
    fn default_blank_runs_before_between_and_after_content_read_exactly() {
        let pane = terminal(12, 4, VERY_DEEP_HISTORY_BYTES);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();

        // Nothing but blank lines: the read is empty.
        write_blank_lines(&pane, BLANK_RUN);
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        assert!(!cache.has_text());
        assert!(cache.pieces().is_empty());

        write(&pane, b"first\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        assert!(cache.has_text());
        write_blank_lines(&pane, BLANK_RUN);
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        write(&pane, b"second\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        assert!(
            holds_blank_chunk(&cache),
            "the run is cached as blank chunks"
        );
        // Trailing blank lines are formatted and cached but not exposed.
        write_blank_lines(&pane, BLANK_RUN);
        let read = source.read(&mut cache);
        assert_eq!(read, whole_read(&pane));
        assert!(holds_blank_chunk(&cache));
        assert!(read.is_some_and(|text| text.ends_with("second")));
        // The pieces are the same view.
        assert!(
            cache
                .pieces()
                .last()
                .is_some_and(|piece| !piece.text.ends_with("\r\n"))
        );
    }

    #[test]
    fn painted_and_hyperlinked_blank_rows_are_content() {
        let pane = terminal(12, 4, VERY_DEEP_HISTORY_BYTES);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        write(&pane, b"top\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));

        // Every kind of blank the formatter keeps, in runs longer than a
        // window, each followed by default blank lines that are not content.
        let kinds: [&str; 5] = [
            "\x1b[42m   \x1b[0m",
            "\x1b[4m   \x1b[0m",
            "\x1b[7m   \x1b[0m",
            "\x1b[9m   \x1b[0m",
            "\x1b]8;;https://example.test/blank\x1b\\   \x1b]8;;\x1b\\",
        ];
        for kind in kinds {
            let run = format!("{kind}\r\n").repeat(BLANK_RUN);
            write(&pane, run.as_bytes());
            assert_eq!(source.read(&mut cache), whole_read(&pane), "{kind:?}");
            write_blank_lines(&pane, 40);
            assert_eq!(source.read(&mut cache), whole_read(&pane), "{kind:?}");
        }
        // The painted rows are content, so the read runs past the last text.
        assert!(cache.text().contains("https://example.test/blank"));
        assert!(
            cache
                .chunks
                .iter()
                .any(|chunk| chunk.content_end.is_some_and(|end| end > 0))
        );
        // A painted run at the very end, with no blank line after it.
        write(&pane, "\x1b[42m   \x1b[0m".as_bytes());
        assert_eq!(source.read(&mut cache), whole_read(&pane));
    }

    #[test]
    fn blank_continuations_of_wrapped_lines_read_exactly() {
        let pane = terminal(12, 4, VERY_DEEP_HISTORY_BYTES);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        // Fill exactly one window of rows with one line, then one character
        // more, erased: the row that continues the line is blank, and the
        // chunk boundary falls on the last soft-wrapped row.
        let window = usize::try_from(SCAN_CHUNK_ROWS).expect("test precondition");
        write(&pane, "w".repeat(12 * window + 1).as_bytes());
        write(&pane, b"\x1b[2K\r\n");
        write_blank_lines(&pane, 60);
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        // The line's closing fragment is the blank continuation, with the
        // blank lines after it, and it counts as content.
        assert!(
            cache
                .chunks
                .iter()
                .any(|chunk| chunk.resumed && chunk.content_end == Some(0)),
            "a chunk only finishes the wrapped line"
        );
        // Blank lines after the line, then more text, on later saves.
        write_blank_lines(&pane, 40);
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        write(&pane, b"later\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        write_blank_lines(&pane, 40);
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        assert_eq!(Some(cache.text()), whole_read(&pane));
    }

    #[test]
    fn a_wrapped_line_with_blank_rows_after_a_cut_reads_exactly() {
        // Wrapped rows whose continuation rows are blank land on window ends
        // in every alignment.
        for offset in [0, 1, 5, 11] {
            let pane = terminal(12, 4, VERY_DEEP_HISTORY_BYTES);
            let source = PaneHistorySource(Arc::clone(&pane));
            let mut cache = PaneHistoryCache::default();
            write_blank_lines(&pane, offset);
            for round in 0..6 {
                write(&pane, format!("\x1b[3{}m", round + 1).as_bytes());
                write(&pane, "x".repeat(12 * 700 + 3).as_bytes());
                write(&pane, b"\x1b[2K\x1b[0m\r\n");
                write_blank_lines(&pane, 300);
                assert_eq!(
                    source.read(&mut cache),
                    whole_read(&pane),
                    "offset {offset} round {round}"
                );
            }
        }
    }

    #[test]
    fn evicting_the_last_content_leaves_blank_chunks_and_no_text() {
        // The smallest history keeps a thousand lines.
        let pane = terminal(12, 4, 2048);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        write(&pane, b"only content\r\n");
        assert_eq!(source.read(&mut cache), whole_read(&pane));
        assert!(cache.has_text());
        for round in 0..30 {
            write_blank_lines(&pane, 100);
            assert_eq!(source.read(&mut cache), whole_read(&pane), "round {round}");
        }
        assert!(
            shepr_vt::lock_terminal_core(&pane.core)
                .expect("test precondition")
                .terminal
                .history_origin()
                > AbsRow(0),
            "the test must evict the content"
        );
        assert!(!cache.chunks.is_empty());
        assert!(cache.chunks.iter().all(|chunk| chunk.content_end.is_none()));
        assert!(!cache.has_text());
        assert!(cache.pieces().is_empty());
        assert_eq!(cache.text(), "");
    }

    /// No two neighbours the caps would let merge: what bounds the chunk
    /// count by the history size and not by the number of saves.
    fn assert_no_mergeable_neighbours(cache: &PaneHistoryCache) {
        for (index, pair) in cache
            .chunks
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .enumerate()
        {
            assert!(!pair[0].can_absorb(pair[1]), "chunks {index} and next");
        }
    }

    /// What one small save writes: blank lines, styled text, wrapped lines and
    /// a line left open across saves, so merged neighbours are closed and open,
    /// resumed, blank and contentful in every mix.
    fn small_save(round: usize) -> String {
        match round % 5 {
            0 => "\r\n\r\n".to_string(),
            1 => format!("\x1b[1;31mred {round}\x1b[0m\r\nplain {round}\r\n"),
            // The line stays open: no newline, styled, wraps in history.
            2 => format!("\x1b[4m{}", "u".repeat(30)),
            3 => format!("{}\x1b[0m\r\n", "v".repeat(20)),
            _ => format!("a long line that wraps {round}\r\n"),
        }
    }

    #[test]
    fn a_long_run_of_small_saves_keeps_a_bounded_chunk_count_and_reads_exactly() {
        let pane = terminal(12, 4, VERY_DEEP_HISTORY_BYTES);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        let saves = 1_500;
        for round in 0..saves {
            write(&pane, small_save(round).as_bytes());
            assert_eq!(source.read(&mut cache), whole_read(&pane), "round {round}");
            assert_no_mergeable_neighbours(&cache);
        }
        // Blank saves at the end: trailing blank rows merge in, unexposed.
        for round in 0..50 {
            write_blank_lines(&pane, 2);
            assert_eq!(source.read(&mut cache), whole_read(&pane), "blank {round}");
        }
        assert_no_mergeable_neighbours(&cache);

        let rows = cache
            .chunks
            .back()
            .map_or(0, |chunk| chunk.end.0)
            .saturating_sub(cache.chunks.front().map_or(0, |chunk| chunk.start.0));
        assert!(rows > 3_000, "history of {rows} rows");
        // One chunk per save would be well over a thousand.
        let bound = usize::try_from(2 * rows / MERGE_MAX_ROWS + 2).expect("test precondition");
        assert!(
            cache.chunks.len() <= bound,
            "{} chunks for {rows} rows",
            cache.chunks.len()
        );
        assert!(cache.pieces().len() <= bound + 1);
        assert!(cache.chunks.iter().any(|chunk| chunk.rows() > 100));
        assert!(
            cache.chunks.iter().any(|chunk| chunk.open.is_some()),
            "an open chunk was cached"
        );
        assert_eq!(Some(cache.text()), whole_read(&pane));
    }

    #[test]
    fn eviction_inside_a_merged_chunk_reads_exactly() {
        // The smallest history keeps a thousand lines, so the origin walks
        // through merged chunks a few rows per save.
        let pane = terminal(12, 4, 2048);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        let mut inside_merged = 0;
        for round in 0..1_500 {
            write(&pane, small_save(round).as_bytes());
            let origin = shepr_vt::lock_terminal_core(&pane.core)
                .expect("test precondition")
                .terminal
                .history_origin();
            // The origin is past the first rows of a chunk that merged more
            // than one save's rows: part of it is evicted, the rest stays.
            if cache
                .chunks
                .iter()
                .any(|chunk| chunk.rows() > 6 && chunk.start < origin && origin < chunk.end)
            {
                inside_merged += 1;
            }
            assert_eq!(source.read(&mut cache), whole_read(&pane), "round {round}");
            assert_no_mergeable_neighbours(&cache);
            assert!(
                cache
                    .chunks
                    .iter()
                    .all(|chunk| chunk.rows() <= MERGE_MAX_ROWS)
            );
        }
        assert!(
            inside_merged > 20,
            "evicted inside a merged chunk {inside_merged} times"
        );
        assert!(cache.chunks.len() <= 12, "{} chunks", cache.chunks.len());
        assert_eq!(Some(cache.text()), whole_read(&pane));
    }

    #[test]
    fn merging_joins_content_ends_as_pieces_cut_them() {
        fn chunk(
            start: u64,
            text: &str,
            content_end: Option<usize>,
            resumed: bool,
            open: bool,
        ) -> HistoryChunk {
            HistoryChunk {
                start: AbsRow(start),
                end: AbsRow(start + 1),
                text: text.into(),
                content_end,
                resumed,
                open: open.then(AnsiCarry::default),
            }
        }
        let cases = [
            // closed + closed: the separator is part of the merged text.
            (
                chunk(0, "ab", Some(2), false, false),
                chunk(1, "cd", Some(2), false, false),
            ),
            (
                chunk(0, "ab\r\n\r\n", Some(2), false, false),
                chunk(1, "cd", Some(2), false, false),
            ),
            // A trailing blank second chunk leaves the first's content end.
            (
                chunk(0, "ab\r\n", Some(2), false, false),
                chunk(1, "\r\n", None, false, false),
            ),
            (
                chunk(0, "\r\n", None, false, false),
                chunk(1, "\r\n", None, false, false),
            ),
            (
                chunk(0, "\r\n", None, false, false),
                chunk(1, "cd", Some(2), false, false),
            ),
            // open + resumed: no separator, and Some(0) lands at the join.
            (
                chunk(0, "abc", Some(3), false, true),
                chunk(1, "", Some(0), true, false),
            ),
            (
                chunk(0, "abc", Some(3), false, true),
                chunk(1, "de\r\n", Some(2), true, false),
            ),
            (
                chunk(0, "abc", Some(3), false, true),
                chunk(1, "de", Some(2), true, true),
            ),
        ];
        for (index, (first, second)) in cases.into_iter().enumerate() {
            let mut cache = PaneHistoryCache::default();
            let mut merged_cache = PaneHistoryCache::default();
            let (first_resumed, second_open) = (first.resumed, second.open.is_some());
            let expect_text = format!("{}{}{}", first.text, first.separator(), second.text);
            let copy = |c: &HistoryChunk| HistoryChunk {
                start: c.start,
                end: c.end,
                text: Arc::clone(&c.text),
                content_end: c.content_end,
                resumed: c.resumed,
                open: c.open.clone(),
            };
            merged_cache.chunks.push_back(copy(&first));
            merged_cache.chunks.push_back(copy(&second));
            cache.chunks.push_back(first);
            cache.chunks.push_back(second);
            assert!(cache.chunks[0].can_absorb(&cache.chunks[1]));
            merged_cache.coalesce_at(1);
            assert_eq!(merged_cache.chunks.len(), 1, "case {index}");
            let merged = &merged_cache.chunks[0];
            assert_eq!(&*merged.text, expect_text, "case {index}");
            assert_eq!(merged.resumed, first_resumed, "case {index}");
            assert_eq!(merged.open.is_some(), second_open, "case {index}");
            assert_eq!(merged.end, AbsRow(2));
            // As pieces cut them, unmerged and merged (the tail is empty).
            assert_eq!(merged_cache.text(), cache.text(), "case {index}");
            assert_eq!(merged_cache.has_text(), cache.has_text(), "case {index}");
        }
    }

    #[test]
    fn an_alternate_screen_save_keeps_the_trimmed_primary_read() {
        let pane = terminal(12, 4, VERY_DEEP_HISTORY_BYTES);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        write(&pane, b"head\r\n");
        write_blank_lines(&pane, BLANK_RUN);
        write(&pane, b"tail\r\n");
        write_blank_lines(&pane, BLANK_RUN);
        let before = source.read(&mut cache);
        assert_eq!(before, whole_read(&pane));
        assert!(before.as_ref().is_some_and(|text| text.ends_with("tail")));
        let revision = cache.revision();

        write(&pane, b"\x1b[?1049hFULL SCREEN");
        assert_eq!(source.read(&mut cache), None);
        // The cache still exposes the last primary read, trimmed as before,
        // and did not change.
        assert_eq!(Some(cache.text()), before);
        assert_eq!(cache.revision(), revision);
        assert!(cache.has_text());

        write(&pane, b"\x1b[?1049l");
        assert_eq!(source.read(&mut cache), whole_read(&pane));
    }

    #[test]
    fn an_unreadable_replacement_terminal_leaves_the_previous_cache_untouched() {
        let pane = terminal(20, 4, 4096);
        let source = PaneHistorySource(Arc::clone(&pane));
        let mut cache = PaneHistoryCache::default();
        write(&pane, b"history before replacement\r\n");
        let previous = source.read(&mut cache).expect("primary screen is readable");
        let revision = cache.revision();

        let replacement = terminal(20, 4, 4096);
        // A thread that panics while holding the core lock poisons it, so the
        // replacement terminal cannot be read.
        let holder = Arc::clone(&replacement);
        let poisoned = std::thread::spawn(move || {
            let _guard = holder.core.lock().expect("test mutex starts unpoisoned");
            panic!("poison replacement terminal");
        })
        .join();
        assert!(poisoned.is_err());

        assert!(!PaneHistorySource(replacement).refresh(&mut cache));
        assert_eq!(cache.text(), previous);
        assert_eq!(cache.revision(), revision);
    }
}
