use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    Clean,
    Partial,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorVisualStyle {
    Bar,
    Block,
    Underline,
    BlockHollow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorViewport {
    pub at: Point<ViewportRow>,
    pub wide_tail: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCursor {
    pub viewport: Option<CursorViewport>,
    pub visible: bool,
    pub blinking: bool,
    pub visual_style: CursorVisualStyle,
}

impl Terminal {
    fn render_cursor(&self) -> RenderCursor {
        let grid = self.emu.term.grid();
        let point = grid.cursor.point;
        let display_offset = i64::try_from(grid.display_offset()).unwrap_or(i64::MAX);
        let viewport_y = i64::from(point.line.0) + display_offset;
        // Guarded by `viewport_y >= 0` below, so this never truncates in the branch that uses it.
        let viewport_y_usize = usize::try_from(viewport_y).unwrap_or(0);
        let viewport = (viewport_y >= 0
            && viewport_y_usize < grid.screen_lines()
            && point.column.0 < grid.columns())
        .then(|| CursorViewport {
            at: Point::new(
                ViewportRow(saturating_u16(viewport_y_usize)),
                saturating_u16(point.column.0),
            ),
            wide_tail: grid[point].flags.contains(Flags::WIDE_CHAR_SPACER),
        });
        let style = self.emu.term.cursor_style();
        RenderCursor {
            viewport,
            visible: self.emu.term.mode().contains(TermMode::SHOW_CURSOR),
            blinking: style.blinking,
            visual_style: match style.shape {
                CursorShape::Block | CursorShape::Hidden => CursorVisualStyle::Block,
                CursorShape::Underline => CursorVisualStyle::Underline,
                CursorShape::Beam => CursorVisualStyle::Bar,
                CursorShape::HollowBlock => CursorVisualStyle::BlockHollow,
            },
        }
    }
}

#[derive(Default)]
struct RowSnapshot {
    cells: Vec<Cell>,
    dirty: bool,
}

/// A snapshot of the viewport for rendering. Row dirty flags accumulate across
/// [`RenderState::update`] calls until a dirty-row transaction commits.
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
                foreground_source: ColorSource::Builtin,
                background_source: ColorSource::Builtin,
                child_palette: [None; shepr_core::limits::PALETTE_COLOR_COUNT],
            },
        }
    }

    pub fn update(&mut self, terminal: &Terminal) {
        let grid = terminal.emu.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let display_offset = grid.display_offset();
        let dims_changed = cols != self.cols || rows != self.rows.len();
        if dims_changed {
            self.cols = cols;
            self.rows = (0..rows).map(|_| RowSnapshot::default()).collect();
        }
        let full = dims_changed || terminal.damage.full_since(self.seen_generation);
        let mut any_changed = false;
        for (y, snapshot) in self.rows.iter_mut().enumerate() {
            let changed = full || terminal.damage.row_since(y, self.seen_generation);
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
            snapshot.dirty = true;
            any_changed = true;
        }
        if full {
            self.dirty = Dirty::Full;
        } else if any_changed && self.dirty == Dirty::Clean {
            self.dirty = Dirty::Partial;
        }
        self.seen_generation = terminal.damage.generation();
        self.cursor = terminal.render_cursor();
        self.colors = terminal.render_colors();
    }

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

    /// Borrows pending rows. Dropping without committing preserves all damage.
    pub fn take_dirty_rows(&mut self, max_rows: u16) -> DirtyRows<'_> {
        DirtyRows {
            state: self,
            max_rows,
        }
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

impl Default for RenderState {
    fn default() -> Self {
        Self::new()
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
            if self.dirty_only && self.state.dirty != Dirty::Full && !snapshot.dirty {
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
    pub fn y(&self) -> ViewportRow {
        ViewportRow(saturating_u16(self.index))
    }

    pub fn is_dirty(&self) -> bool {
        self.snapshot.dirty
    }

    pub fn cells(&self) -> impl Iterator<Item = CellView<'a>> + 'a {
        let colors = self.colors;
        self.snapshot
            .cells
            .iter()
            .map(move |cell| CellView { cell, colors })
    }
}

/// A damage transaction with no row allocation. Only a successful collector
/// commits; an early return leaves the complete pending damage intact.
pub struct DirtyRows<'a> {
    state: &'a mut RenderState,
    max_rows: u16,
}

impl DirtyRows<'_> {
    pub fn rows(&self) -> impl Iterator<Item = RowView<'_>> {
        self.state
            .dirty_rows()
            .take_while(|row| row.y().0 < self.max_rows)
    }

    pub fn commit(self) {
        let mut remaining = false;
        for (y, row) in self.state.rows.iter_mut().enumerate() {
            if y < usize::from(self.max_rows) {
                row.dirty = false;
            } else {
                remaining |= row.dirty;
            }
        }
        self.state.dirty = if remaining {
            Dirty::Partial
        } else {
            Dirty::Clean
        };
    }
}

#[cfg(test)]
impl RenderState {
    pub(crate) fn cols(&self) -> u16 {
        saturating_u16(self.cols)
    }

    pub(crate) fn clean(&mut self) {
        self.dirty = Dirty::Clean;
        for row in &mut self.rows {
            row.dirty = false;
        }
    }
}
