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
//! Chunks join exactly as one whole read would format them: a chunk starts on
//! a logical line and ends on a row that ends one and has visible text, so the
//! formatter's trailing-blank-line trim removes nothing from it, and the
//! formatter closes all SGR and hyperlink state at every line end. The whole
//! read is the chunks and the screen read joined with the line separator.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use super::*;

/// Chunks of formatted history a reader keeps between reads of one pane,
/// oldest first and contiguous. Only a reader of the same terminal may use
/// it: a read through another terminal's source starts it afresh.
///
/// The cache also keeps the screen part of the last successful read, so the
/// pane's last primary history can be rebuilt (`text`) while the alternate
/// screen hides it, and a revision that changes exactly when that text does,
/// so a save can tell that nothing changed without comparing text.
#[derive(Default)]
pub struct PaneHistoryCache {
    terminal: Weak<PaneTerminal>,
    epoch: u64,
    cols: u16,
    chunks: VecDeque<HistoryChunk>,
    /// The screen part of the last successful read.
    tail: String,
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
/// of which has visible text.
struct HistoryChunk {
    start: AbsRow,
    end: AbsRow,
    text: String,
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
            self.tail.clear();
            self.revision = next_revision();
        }
    }

    /// Records the screen part of a read.
    fn set_tail(&mut self, tail: String) {
        if self.tail != tail {
            self.tail = tail;
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

    /// The cached chunks followed by the tail of the last read, joined as one
    /// read would join their lines: the pane's primary history as of the last
    /// successful read, even while the alternate screen hides it now.
    pub fn text(&self) -> String {
        let cached: usize = self.chunks.iter().map(|chunk| chunk.text.len() + 2).sum();
        let mut text = String::with_capacity(cached + self.tail.len());
        for chunk in &self.chunks {
            if !text.is_empty() {
                text.push_str("\r\n");
            }
            text.push_str(&chunk.text);
        }
        if !self.tail.is_empty() {
            if !text.is_empty() {
                text.push_str("\r\n");
            }
            text.push_str(&self.tail);
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

/// VT text of absolute rows `start..=last`, unwrapped, trailing blank lines
/// trimmed.
fn format_rows(terminal: &shepr_vt::Terminal, start: AbsRow, last: AbsRow) -> Option<String> {
    let cols = terminal.cols();
    let start = terminal.screen_row_for_absolute(start)?;
    let last = terminal.screen_row_for_absolute(last)?;
    terminal
        .read_ansi_screen(
            Point::new(start, 0),
            Point::new(last, cols.saturating_sub(1)),
            false,
            true,
        )
        .ok()
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
            // their own, which ends where the next chunk starts.
            if let Some(front) = cache.chunks.front()
                && front.start > bounds.origin
            {
                let end = front.start;
                let Some(text) = format_rows(terminal, bounds.origin, end.saturating_sub(1)) else {
                    cache.clear();
                    return None;
                };
                cache.push_front(HistoryChunk {
                    start: bounds.origin,
                    end,
                    text,
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
                match chunk_boundary(terminal, from, window_end) {
                    Some(end) => {
                        let Some(text) = format_rows(terminal, next, end.saturating_sub(1)) else {
                            cache.clear();
                            return None;
                        };
                        cache.push_back(HistoryChunk {
                            start: next,
                            end,
                            text,
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

            // The cache reaches the last logical line that ends in history;
            // the rest is read now, under this hold, up to the last row with
            // content or the cursor.
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
            let tail = match terminal.screen_row_for_absolute(next) {
                Some(start) if start.0 <= end => {
                    let Some(tail) = format_rows(
                        terminal,
                        next,
                        terminal.absolute_row_for_screen(ScreenRow(end)),
                    ) else {
                        // Chunks may have advanced past the old tail.
                        cache.clear();
                        return None;
                    };
                    tail
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
}
