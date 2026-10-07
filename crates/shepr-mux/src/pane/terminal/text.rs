use super::*;
use crate::limits::{
    MAX_PARAGRAPH_MOTION_ROWS, MAX_WORD_MOTION_ROWS, WORD_MOTION_INITIAL_WINDOW_ROWS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextClass {
    Whitespace,
    Separator,
    Word,
}

/// A word-motion unit: one cell's text (or a line break when `point` is
/// `None`). Rows are absolute.
#[derive(Debug)]
struct TextAtom {
    point: Option<TerminalTextPoint>,
    end_col: u16,
    class: TextClass,
}

/// Where one cell's text sits in a [`LogicalTextLine`]. Rows are absolute.
#[derive(Debug)]
struct TextSpan {
    byte_start: usize,
    byte_end: usize,
    start: TerminalTextPoint,
    end: TerminalTextPoint,
}

/// The text of one hard line (soft-wrapped rows joined), trailing blanks
/// trimmed, with the cell each byte range came from.
#[derive(Debug, Default)]
pub(super) struct LogicalTextLine {
    text: String,
    spans: Vec<TextSpan>,
}

impl LogicalTextLine {
    fn clear(&mut self) {
        self.text.clear();
        self.spans.clear();
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty() && self.spans.is_empty()
    }

    fn trim_end(&mut self) {
        let trimmed_len = self.text.trim_end().len();
        while self
            .spans
            .last()
            .is_some_and(|span| span.byte_start >= trimmed_len)
        {
            self.spans.pop();
        }
        self.text.truncate(trimmed_len);
    }
}

/// Assembles terminal cells, fed row by row, into logical lines (for search)
/// and word atoms (for word motion). Cells arrive as text, so the live
/// terminal can feed it straight from its grid without building per-cell
/// copies, and lines are handed over one at a time as they complete, so a
/// search never holds the whole history's text.
pub(super) struct TextBufferBuilder {
    build_lines: bool,
    build_atoms: bool,
    pub(super) line: LogicalTextLine,
    /// `line` holds a completed line the reader has seen; the next cell
    /// starts a new one.
    line_complete: bool,
    atoms: Vec<TextAtom>,
}

impl TextBufferBuilder {
    pub(super) fn new(build_lines: bool, build_atoms: bool) -> Self {
        Self {
            build_lines,
            build_atoms,
            line: LogicalTextLine::default(),
            line_complete: false,
            atoms: Vec::new(),
        }
    }

    pub(super) fn push_cell(
        &mut self,
        row: AbsRow,
        col: u16,
        wide: shepr_vt::CellWide,
        text: &str,
    ) {
        if self.line_complete {
            self.line.clear();
            self.line_complete = false;
        }
        match wide {
            shepr_vt::CellWide::SpacerTail => {}
            shepr_vt::CellWide::SpacerHead => {
                // The blank a wide character leaves at a soft wrap belongs to
                // the word around it, and to no text.
                if self.build_atoms {
                    let class = self
                        .atoms
                        .last()
                        .map_or(TextClass::Whitespace, |atom| atom.class);
                    self.atoms.push(TextAtom {
                        point: Some(TerminalTextPoint { row, col }),
                        end_col: col,
                        class,
                    });
                }
            }
            shepr_vt::CellWide::Narrow | shepr_vt::CellWide::Wide => {
                let width = wide.columns();
                let start = TerminalTextPoint { row, col };
                let end = TerminalTextPoint {
                    row,
                    col: col.saturating_add(width - 1),
                };
                if self.build_lines {
                    let byte_start = self.line.text.len();
                    self.line.text.push_str(text);
                    let byte_end = self.line.text.len();
                    self.line.spans.push(TextSpan {
                        byte_start,
                        byte_end,
                        start,
                        end,
                    });
                }
                if self.build_atoms {
                    self.atoms.push(TextAtom {
                        point: Some(start),
                        end_col: end.col,
                        class: text_class(text),
                    });
                }
            }
        }
    }

    /// Ends a row. Returns whether it completed a logical line, which is then
    /// in `self.line` until the next cell arrives.
    pub(super) fn end_row(&mut self, soft_wrapped: bool) -> bool {
        if soft_wrapped {
            return false;
        }
        if self.build_lines {
            self.line.trim_end();
            self.line_complete = true;
        }
        if self.build_atoms {
            self.atoms.push(TextAtom {
                point: None,
                end_col: 0,
                class: TextClass::Whitespace,
            });
        }
        true
    }

    /// Forgets a partially assembled line (its first rows were evicted
    /// while a chunked scan had the lock released).
    pub(super) fn discard_line(&mut self) {
        self.line.clear();
        self.line_complete = false;
    }

    /// The text of rows after the last hard line break (the buffer ended on
    /// a soft-wrapped row), untrimmed, if there is any.
    pub(super) fn trailing_line(&self) -> Option<&LogicalTextLine> {
        (self.build_lines && !self.line_complete && !self.line.is_empty()).then_some(&self.line)
    }
}

/// Word atoms over a window of rows.
#[derive(Debug)]
pub(super) struct RetainedTextBuffer {
    atoms: Vec<TextAtom>,
}

impl RetainedTextBuffer {
    /// Word atoms for screen rows `start..end` of the live terminal, with
    /// absolute rows, plus how the first and last row wrap.
    fn live_words(
        terminal: &shepr_vt::Terminal,
        start: usize,
        end: usize,
    ) -> Option<(Self, shepr_vt::RowWrap, shepr_vt::RowWrap)> {
        let mut builder = TextBufferBuilder::new(false, true);
        let mut scratch = String::new();
        let mut first = None;
        let mut last = shepr_vt::RowWrap::default();
        for y in start..end {
            let screen_row = ScreenRow(y);
            let row = terminal.absolute_row_for_screen(screen_row);
            let wrap =
                terminal.visit_screen_row_text(screen_row, &mut scratch, |col, wide, text| {
                    builder.push_cell(row, col, wide, text);
                })?;
            builder.end_row(wrap.soft_wrapped);
            if first.is_none() {
                first = Some(wrap);
            }
            last = wrap;
        }
        let buffer = Self {
            atoms: builder.atoms,
        };
        Some((buffer, first.unwrap_or_default(), last))
    }

    pub(super) fn word_motion(
        &self,
        cursor: TerminalTextPoint,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint> {
        let current = self.atoms.iter().position(|atom| {
            atom.point.is_some_and(|point| {
                point.row == cursor.row && cursor.col >= point.col && cursor.col <= atom.end_col
            })
        })?;
        match motion {
            TerminalWordMotion::NextStart => self.next_word_start(current),
            TerminalWordMotion::PreviousStart => self.previous_word_start(current),
            TerminalWordMotion::NextEnd => self.next_word_end(current),
            TerminalWordMotion::NextBigStart => self.next_big_word_start(current),
            TerminalWordMotion::PreviousBigStart => self.previous_big_word_start(current),
            TerminalWordMotion::NextBigEnd => self.next_big_word_end(current),
        }
    }

    fn next_word_start(&self, current: usize) -> Option<TerminalTextPoint> {
        let current_class = self.atoms.get(current)?.class;
        let mut next = current.saturating_add(1);
        if current_class != TextClass::Whitespace {
            while self
                .atoms
                .get(next)
                .is_some_and(|atom| atom.class == current_class)
            {
                next += 1;
            }
        }
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        self.next_point(next)
    }

    fn previous_word_start(&self, current: usize) -> Option<TerminalTextPoint> {
        let mut previous = current.checked_sub(1)?;
        while self
            .atoms
            .get(previous)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            previous = previous.checked_sub(1)?;
        }
        let class = self.atoms.get(previous)?.class;
        while previous > 0
            && self
                .atoms
                .get(previous - 1)
                .is_some_and(|atom| atom.class == class)
        {
            previous -= 1;
        }
        self.previous_point(previous)
    }

    fn next_word_end(&self, current: usize) -> Option<TerminalTextPoint> {
        let mut next = current.saturating_add(1);
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        let class = self.atoms.get(next)?.class;
        while self
            .atoms
            .get(next + 1)
            .is_some_and(|atom| atom.class == class)
        {
            next += 1;
        }
        self.previous_point(next)
    }

    fn next_big_word_start(&self, current: usize) -> Option<TerminalTextPoint> {
        let mut next = current.saturating_add(1);
        if self
            .atoms
            .get(current)
            .is_some_and(|atom| atom.class != TextClass::Whitespace)
        {
            while self
                .atoms
                .get(next)
                .is_some_and(|atom| atom.class != TextClass::Whitespace)
            {
                next += 1;
            }
        }
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        self.next_point(next)
    }

    fn previous_big_word_start(&self, current: usize) -> Option<TerminalTextPoint> {
        let mut previous = current.checked_sub(1)?;
        while self
            .atoms
            .get(previous)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            previous = previous.checked_sub(1)?;
        }
        while previous > 0
            && self
                .atoms
                .get(previous - 1)
                .is_some_and(|atom| atom.class != TextClass::Whitespace)
        {
            previous -= 1;
        }
        self.previous_point(previous)
    }

    fn next_big_word_end(&self, current: usize) -> Option<TerminalTextPoint> {
        let mut next = current.saturating_add(1);
        while self
            .atoms
            .get(next)
            .is_some_and(|atom| atom.class == TextClass::Whitespace)
        {
            next += 1;
        }
        self.atoms.get(next)?;
        while self
            .atoms
            .get(next + 1)
            .is_some_and(|atom| atom.class != TextClass::Whitespace)
        {
            next += 1;
        }
        self.previous_point(next)
    }

    fn next_point(&self, mut index: usize) -> Option<TerminalTextPoint> {
        while let Some(atom) = self.atoms.get(index) {
            if let Some(point) = atom.point {
                return Some(point);
            }
            index += 1;
        }
        None
    }

    fn previous_point(&self, mut index: usize) -> Option<TerminalTextPoint> {
        loop {
            if let Some(point) = self.atoms.get(index)?.point {
                return Some(point);
            }
            index = index.checked_sub(1)?;
        }
    }

    fn point_is_final_atom(&self, point: TerminalTextPoint) -> bool {
        // Word motion targets are atom start points, so compare against the
        // final atom's start point. Comparing against `end_col` would never
        // match a wide glyph, whose end column is one past its start.
        self.atoms
            .iter()
            .rev()
            .find(|atom| atom.point.is_some())
            .is_some_and(|atom| atom.point == Some(point))
    }
}

fn text_class(text: &str) -> TextClass {
    use shepr_term::word::WordClass;
    let Some(ch) = text.chars().next() else {
        return TextClass::Whitespace;
    };
    match shepr_term::word::classify(ch) {
        WordClass::Whitespace => TextClass::Whitespace,
        WordClass::Separator => TextClass::Separator,
        WordClass::Word => TextClass::Word,
    }
}

fn text_fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// One copy-mode search: logical lines are fed in reading order and only the
/// matches that can end up in the returned window are kept, so memory stays
/// bounded by the window size however long the history is.
pub(super) struct TextSearch {
    regex: regex::Regex,
    window: MatchWindow,
}

impl TextSearch {
    pub(super) fn new(search: TerminalTextSearch<'_>) -> Option<Self> {
        if search.query.is_empty() || search.limit.get() == 0 {
            return None;
        }
        let regex = regex::RegexBuilder::new(&regex::escape(search.query))
            .case_insensitive(!search.case.is_sensitive(search.query))
            .build()
            .ok()?;
        let origin = match search.direction {
            TerminalSearchDirection::Forward => {
                search.previous.map_or(search.cursor, |range| range.end)
            }
            TerminalSearchDirection::Backward => {
                search.previous.map_or(search.cursor, |range| range.start)
            }
        };
        Some(Self {
            regex,
            window: MatchWindow {
                direction: search.direction,
                origin,
                limit: search.limit.get(),
                total: 0,
                target: None,
                first: Vec::new(),
                recent: VecDeque::new(),
                boundary: None,
                after: Vec::new(),
            },
        })
    }

    pub(super) fn scan_line(
        &mut self,
        line: &LogicalTextLine,
        cols: u16,
        screen: shepr_vt::ActiveScreen,
    ) {
        for found in self.regex.find_iter(&line.text) {
            // Only matches that start and end on cell boundaries count: a
            // query for a lone combining mark must not match inside a cell.
            let Ok(start) = line
                .spans
                .binary_search_by_key(&found.start(), |span| span.byte_start)
            else {
                continue;
            };
            let Ok(end) = line
                .spans
                .binary_search_by_key(&found.end(), |span| span.byte_end)
            else {
                continue;
            };
            self.window.push(TerminalTextMatch {
                start: line.spans[start].start,
                end: line.spans[end].end,
                source_fingerprint: text_fingerprint(found.as_str()),
                scan_cols: cols,
                scan_screen: screen,
            });
        }
    }

    pub(super) fn finish(self) -> TerminalSearchWindow {
        self.window.finish()
    }
}

/// The part of a search's match list that can end up in its window.
///
/// The target is the first match after the origin (forward) or the last one
/// before it (backward); without one, a forward search wraps to the first
/// match and a backward one to the last. The window is `limit` matches
/// around the target, so it never reaches more than `limit` matches either
/// side of it. Matches arrive in reading order, so the target is known as
/// soon as the scan passes the origin: until then the last `limit` matches
/// are kept, from the target on the next `limit`, and the first `limit`
/// always (for a forward wrap).
pub(super) struct MatchWindow {
    pub(super) direction: TerminalSearchDirection,
    pub(super) origin: TerminalTextPoint,
    pub(super) limit: usize,
    pub(super) total: usize,
    pub(super) target: Option<usize>,
    pub(super) first: Vec<TerminalTextMatch>,
    /// The last `limit` matches before `boundary` (before the end while no
    /// boundary is set).
    pub(super) recent: VecDeque<TerminalTextMatch>,
    /// Index of the first match kept in `after`, once the target is known.
    pub(super) boundary: Option<usize>,
    pub(super) after: Vec<TerminalTextMatch>,
}

impl MatchWindow {
    pub(super) fn push(&mut self, text_match: TerminalTextMatch) {
        let index = self.total;
        self.total = self.total.saturating_add(1);
        if self.first.len() < self.limit {
            self.first.push(text_match);
        }
        if self.boundary.is_none() {
            match self.direction {
                TerminalSearchDirection::Forward => {
                    if text_match.start > self.origin {
                        self.target = Some(index);
                        self.boundary = Some(index);
                    }
                }
                TerminalSearchDirection::Backward => {
                    if text_match.end < self.origin {
                        self.target = Some(index);
                    } else if self.target.is_some() {
                        // Match ends only grow, so no later match can be the
                        // target. Without a target the search wraps to the
                        // last match, which the recent matches keep tracking.
                        self.boundary = Some(index);
                    }
                }
            }
        }
        if self.boundary.is_some() {
            if self.after.len() < self.limit {
                self.after.push(text_match);
            }
        } else {
            if self.recent.len() == self.limit {
                self.recent.pop_front();
            }
            self.recent.push_back(text_match);
        }
    }

    pub(super) fn get(&self, index: usize) -> Option<TerminalTextMatch> {
        if let Some(text_match) = self.first.get(index) {
            return Some(*text_match);
        }
        if let Some(boundary) = self.boundary
            && index >= boundary
        {
            return self.after.get(index - boundary).copied();
        }
        let recent_start = self
            .boundary
            .unwrap_or(self.total)
            .saturating_sub(self.recent.len());
        self.recent.get(index.checked_sub(recent_start)?).copied()
    }

    pub(super) fn finish(self) -> TerminalSearchWindow {
        let total = self.total;
        if total == 0 {
            return TerminalSearchWindow::empty();
        }
        let target = self.target.unwrap_or(match self.direction {
            TerminalSearchDirection::Forward => 0,
            TerminalSearchDirection::Backward => total - 1,
        });
        let retained = self.limit.min(total);
        let start = target
            .saturating_sub(retained / 2)
            .min(total.saturating_sub(retained));
        let end = start.saturating_add(retained);
        TerminalSearchWindow {
            matches: (start..end).filter_map(|index| self.get(index)).collect(),
            current: Some(TerminalSearchPosition {
                window_index: target - start,
                global_index: target,
            }),
            total,
            complete: true,
        }
    }
}

/// Word motion on the live terminal, with absolute rows. Reads a window of
/// rows around the start and widens it while the answer may lie past its
/// edge (no target yet, or a word continuing across a soft wrap at the
/// window's edge). The caller holds the terminal lock, so the window stops
/// at `MAX_WORD_MOTION_ROWS`, whose far edge then counts as the end of the
/// history.
pub(super) fn word_motion_in(
    terminal: &shepr_vt::Terminal,
    point: TerminalTextPoint,
    motion: TerminalWordMotion,
) -> Option<TerminalTextPoint> {
    let total_rows = terminal.total_rows();
    let row = terminal.screen_row_for_absolute(point.row)?.0;
    let backward = matches!(
        motion,
        TerminalWordMotion::PreviousStart | TerminalWordMotion::PreviousBigStart
    );
    let to_word_end = matches!(
        motion,
        TerminalWordMotion::NextEnd | TerminalWordMotion::NextBigEnd
    );
    let mut window_rows = WORD_MOTION_INITIAL_WINDOW_ROWS.min(MAX_WORD_MOTION_ROWS);
    loop {
        let (start_row, end_row) = if backward {
            (row.saturating_sub(window_rows.saturating_sub(1)), row + 1)
        } else {
            (row, row.saturating_add(window_rows).min(total_rows))
        };
        let (buffer, first, last) = RetainedTextBuffer::live_words(terminal, start_row, end_row)?;
        let starts_in_continuation = first.wrap_continuation && start_row > 0;
        let ends_in_continuation = last.soft_wrapped && end_row < total_rows;
        let target = buffer.word_motion(point, motion);
        let needs_more_history = backward
            && starts_in_continuation
            && target.is_some_and(|target| {
                target.row == terminal.absolute_row_for_screen(ScreenRow(start_row))
            });
        let needs_more_future = to_word_end
            && ends_in_continuation
            && target.is_some_and(|target| buffer.point_is_final_atom(target));
        if target.is_some() && !needs_more_history && !needs_more_future {
            return target;
        }
        let reached_edge = if backward {
            start_row == 0
        } else {
            end_row == total_rows
        };
        if reached_edge || window_rows >= MAX_WORD_MOTION_ROWS {
            return target;
        }
        window_rows = window_rows
            .saturating_mul(2)
            .min(total_rows)
            .min(MAX_WORD_MOTION_ROWS);
    }
}

/// The next blank row above or below the cursor, preserving its column and
/// looking at most `MAX_PARAGRAPH_MOTION_ROWS` rows away.
pub(super) fn paragraph_motion_in(
    terminal: &shepr_vt::Terminal,
    cursor: TerminalTextPoint,
    motion: TerminalParagraphMotion,
) -> Option<TerminalTextPoint> {
    let total_rows = terminal.total_rows();
    let current = terminal.screen_row_for_absolute(cursor.row)?.0;
    let mut scratch = String::new();
    // The history's edges end the walk inside the loop (no row before 0, or
    // past the last), so the range bounds only the distance.
    for distance in 1..=MAX_PARAGRAPH_MOTION_ROWS {
        let candidate = match motion {
            TerminalParagraphMotion::Previous => current.checked_sub(distance)?,
            TerminalParagraphMotion::Next => {
                let candidate = current.saturating_add(distance);
                if candidate >= total_rows {
                    return None;
                }
                candidate
            }
        };
        let mut blank = true;
        terminal.visit_screen_row_text(ScreenRow(candidate), &mut scratch, |_, _, text| {
            blank &= text.chars().all(char::is_whitespace);
        })?;
        if blank {
            return Some(TerminalTextPoint {
                row: terminal.absolute_row_for_screen(ScreenRow(candidate)),
                col: cursor.col,
            });
        }
    }
    None
}

#[cfg(test)]
fn terminal_cell_text(graphemes: &[u32]) -> String {
    if graphemes.is_empty() {
        return " ".to_string();
    }
    graphemes
        .iter()
        .map(|codepoint| char::from_u32(*codepoint).unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Word atoms plus the logical lines over owned rows 0.., for exercising
/// search and word motion on hand-built cells; the live terminal streams
/// straight from its grid.
#[cfg(test)]
#[derive(Debug)]
pub(super) struct OwnedTextBuffer {
    words: RetainedTextBuffer,
    cols: u16,
    lines: Vec<LogicalTextLine>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct OwnedTextCell {
    pub(super) wide: shepr_vt::CellWide,
    pub(super) graphemes: Vec<u32>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct OwnedTextRow {
    pub(super) cells: Vec<OwnedTextCell>,
    pub(super) soft_wrapped: bool,
}

#[cfg(test)]
impl OwnedTextBuffer {
    pub(super) fn new(cols: u16, rows: Vec<OwnedTextRow>) -> Self {
        let mut builder = TextBufferBuilder::new(true, true);
        let mut lines = Vec::new();
        for (row, screen_row) in (0u64..).zip(rows) {
            let row = AbsRow(row);
            for (col, cell) in (0u16..).zip(&screen_row.cells) {
                builder.push_cell(row, col, cell.wide, &terminal_cell_text(&cell.graphemes));
            }
            if builder.end_row(screen_row.soft_wrapped) {
                lines.push(std::mem::take(&mut builder.line));
            }
        }
        if builder.trailing_line().is_some() {
            lines.push(std::mem::take(&mut builder.line));
        }
        Self {
            words: RetainedTextBuffer {
                atoms: builder.atoms,
            },
            cols,
            lines,
        }
    }

    pub(super) fn from_terminal(terminal: &shepr_vt::Terminal) -> Self {
        let mut rows = Vec::with_capacity(terminal.total_rows());
        let mut scratch = String::new();
        for row in 0..terminal.total_rows() {
            let mut cells = Vec::new();
            let wrap = terminal
                .visit_screen_row_text(shepr_vt::ScreenRow(row), &mut scratch, |_, wide, text| {
                    cells.push(OwnedTextCell {
                        wide,
                        graphemes: text.chars().map(u32::from).collect(),
                    });
                })
                .expect("test precondition");
            rows.push(OwnedTextRow {
                cells,
                soft_wrapped: wrap.soft_wrapped,
            });
        }
        Self::new(terminal.cols(), rows)
    }

    pub(super) fn word_motion(
        &self,
        row: AbsRow,
        col: u16,
        motion: TerminalWordMotion,
    ) -> Option<TerminalTextPoint> {
        self.words
            .word_motion(TerminalTextPoint { row, col }, motion)
    }

    pub(super) fn search_window(
        &self,
        active_screen: shepr_vt::ActiveScreen,
        search: TerminalTextSearch<'_>,
    ) -> TerminalSearchWindow {
        let Some(mut text_search) = TextSearch::new(search) else {
            return TerminalSearchWindow::empty();
        };
        for line in &self.lines {
            text_search.scan_line(line, self.cols, active_screen);
        }
        text_search.finish()
    }
}
