//! Size a pane's PTY is spawned at.
//!
//! A child reads its window size at startup (argv panes and agents often only
//! once), so a new pane must be spawned at the size view computation will give
//! it, not at some other pane's size. The view computes its pane rects through
//! the same `PaneGeometry::visible_panes` (BSP split, chrome, the zoomed case), and
//! the same `pane_inner_rect` and scrollbar gutter.

use ratatui::{
    layout::Rect,
    widgets::{Block, Borders},
};

use shepr_core::layout::{
    NavDirection, PaneId, PaneInfo as LayoutPaneInfo, TileLayout, rect_distance_in_direction,
};

/// The layout model's rect as the one ratatui draws into. Both are plain
/// cell coordinates, so the conversion copies the fields. These are free
/// functions because neither type is local to any shepr crate that sees both,
/// so no `From` impl between them can exist.
fn ratatui_rect(rect: shepr_core::geometry::Rect) -> Rect {
    Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// A ratatui rect as the layout model's rect; the inverse of `ratatui_rect`.
pub fn layout_rect(rect: Rect) -> shepr_core::geometry::Rect {
    shepr_core::geometry::Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// Layout position with the chrome and content geometry added for a view.
/// Construction settles the border-only inner rect. The screen mode and
/// scrollback belong to the surface consumer, so it must settle the gutter
/// with `content_layout` before using this as terminal content geometry.
/// A chrome-only type followed by a finalized content type would enforce
/// that ordering; it requires the surface and retained-surface consumers to
/// adopt the finalization step together.
#[derive(Clone)]
pub struct PaneChromeInfo {
    pub id: PaneId,
    pub rect: Rect,
    pub inner_rect: Rect,
    pub scrollbar_rect: Option<Rect>,
    pub borders: Borders,
    pub is_focused: bool,
}

impl PaneChromeInfo {
    /// Set drawable content and return the reserved scrollbar track. Empty
    /// drawable rects stay empty; the PTY grid minimum belongs to PaneGeometry.
    pub fn content_layout(&mut self, scrollbars: bool, alternate_screen: bool) -> Option<Rect> {
        let inner = pane_inner_rect(self.rect, self.borders);
        self.inner_rect = terminal_content_rect(inner, scrollbars, alternate_screen);
        self.scrollbar_rect = None;
        (self.inner_rect != inner).then(|| {
            Rect::new(
                inner.x.saturating_add(inner.width.saturating_sub(1)),
                inner.y,
                1,
                inner.height,
            )
        })
    }

    /// Whether a reserved track has scrollback to display.
    pub fn scrollbar_visible(max_offset_from_bottom: usize) -> bool {
        max_offset_from_bottom > 0
    }
}

impl From<LayoutPaneInfo> for PaneChromeInfo {
    fn from(pane: LayoutPaneInfo) -> Self {
        Self {
            id: pane.id,
            rect: ratatui_rect(pane.rect),
            inner_rect: ratatui_rect(pane.rect),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: pane.is_focused,
        }
    }
}

impl From<PaneChromeInfo> for LayoutPaneInfo {
    fn from(pane: PaneChromeInfo) -> Self {
        Self {
            id: pane.id,
            rect: layout_rect(pane.rect),
            is_focused: pane.is_focused,
        }
    }
}

pub fn pane_inner_rect(area: Rect, borders: Borders) -> Rect {
    if borders.is_empty() {
        area
    } else {
        Block::default().borders(borders).inner(area)
    }
}

fn touching_neighbor<'a>(
    info: &LayoutPaneInfo,
    panes: &'a [LayoutPaneInfo],
    direction: NavDirection,
) -> Option<&'a LayoutPaneInfo> {
    panes.iter().find(|other| {
        other.id != info.id
            && rect_distance_in_direction(info.rect, other.rect, direction) == Some(0)
    })
}

fn shrink_for_one_cell_gap(size: u16) -> u16 {
    if size > 1 { size - 1 } else { size }
}

pub fn apply_pane_chrome(
    panes: &[LayoutPaneInfo],
    pane_borders: shepr_config::PaneBordersConfig,
    pane_gaps: bool,
    pane_outer_borders: bool,
) -> Vec<PaneChromeInfo> {
    let multi_pane = panes.len() > 1;
    let bordered = pane_borders.shows_borders(multi_pane);
    let outer_left = panes.iter().map(|info| info.rect.x).min().unwrap_or(0);
    let outer_top = panes.iter().map(|info| info.rect.y).min().unwrap_or(0);
    let outer_right = panes
        .iter()
        .map(|info| u32::from(info.rect.x) + u32::from(info.rect.width))
        .max()
        .unwrap_or(0);
    let outer_bottom = panes
        .iter()
        .map(|info| u32::from(info.rect.y) + u32::from(info.rect.height))
        .max()
        .unwrap_or(0);
    panes
        .iter()
        .cloned()
        .map(|layout_info| {
            let right_neighbor = multi_pane
                .then(|| touching_neighbor(&layout_info, panes, NavDirection::Right))
                .flatten();
            let below_neighbor = multi_pane
                .then(|| touching_neighbor(&layout_info, panes, NavDirection::Down))
                .flatten();
            let mut info = PaneChromeInfo::from(layout_info);

            if multi_pane && pane_gaps && !pane_borders.draws_borders() {
                if right_neighbor.is_some() {
                    info.rect.width = shrink_for_one_cell_gap(info.rect.width);
                }
                if below_neighbor.is_some() {
                    info.rect.height = shrink_for_one_cell_gap(info.rect.height);
                }
            }

            info.borders = if !bordered {
                Borders::NONE
            } else {
                let mut borders = Borders::ALL;
                if !pane_gaps {
                    if right_neighbor.is_some() {
                        borders.remove(Borders::RIGHT);
                    }
                    if below_neighbor.is_some() {
                        borders.remove(Borders::BOTTOM);
                    }
                }
                if !pane_outer_borders {
                    if info.rect.x == outer_left {
                        borders.remove(Borders::LEFT);
                    }
                    if info.rect.y == outer_top {
                        borders.remove(Borders::TOP);
                    }
                    if u32::from(info.rect.x) + u32::from(info.rect.width) == outer_right {
                        borders.remove(Borders::RIGHT);
                    }
                    if u32::from(info.rect.y) + u32::from(info.rect.height) == outer_bottom {
                        borders.remove(Borders::BOTTOM);
                    }
                }
                borders
            };
            info.inner_rect = pane_inner_rect(info.rect, info.borders);
            info
        })
        .collect()
}

/// Everything besides the layout tree that decides a pane's content size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneGeometry {
    /// Area the workspace's panes are laid out in.
    pub area: Rect,
    pub pane_borders: shepr_config::PaneBordersConfig,
    pub pane_gaps: bool,
    pub pane_outer_borders: bool,
    pub pane_scrollbars: bool,
}

/// The terminal content rect inside a pane's inner (border-less) rect: one
/// column is kept for the scrollbar gutter unless scrollbars are off, the pane
/// is too narrow, or the terminal is on the alternate screen.
pub fn terminal_content_rect(
    pane_inner: Rect,
    pane_scrollbars: bool,
    alternate_screen: bool,
) -> Rect {
    if !pane_scrollbars || pane_inner.width <= 4 || alternate_screen {
        return pane_inner;
    }
    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

impl PaneGeometry {
    /// The sole pane shown by zoom, shared by ID and geometry projections.
    pub fn zoomed_pane(layout: &TileLayout, zoomed: bool) -> Option<PaneId> {
        zoomed.then(|| layout.focused())
    }

    /// Restore every pane: hidden panes use tiled geometry, while the visible
    /// zoomed pane uses its surface geometry. Resumes start on the primary screen.
    pub fn resume_panes(&self, layout: &TileLayout, zoomed: bool) -> Vec<PaneChromeInfo> {
        let mut panes = self.visible_panes(layout, false);
        if zoomed {
            for visible in self.visible_panes(layout, true) {
                if let Some(pane) = panes.iter_mut().find(|pane| pane.id == visible.id) {
                    *pane = visible;
                }
            }
        }
        for pane in &mut panes {
            pane.content_layout(self.pane_scrollbars, false);
        }
        panes
    }

    /// The visible panes of a workspace with their chrome applied: outer rect and
    /// borders. Call `PaneChromeInfo::content_layout` to settle content using
    /// the pane's screen mode; scroll metrics then select whether its track draws.
    ///
    /// A zoomed workspace shows only its focused pane, filling `area`. Every
    /// edge of that pane is an outer edge, so it is framed on all sides exactly
    /// when borders show for the workspace's real pane count and outer borders
    /// are on.
    /// View computation, background resizing and spawn sizing all go through
    /// here, so the zoomed rule exists once.
    pub fn visible_panes(&self, layout: &TileLayout, zoomed: bool) -> Vec<PaneChromeInfo> {
        let Some(zoomed_pane) = Self::zoomed_pane(layout, zoomed) else {
            return apply_pane_chrome(
                &layout.panes(layout_rect(self.area)),
                self.pane_borders,
                self.pane_gaps,
                self.pane_outer_borders,
            );
        };
        let borders = if self.pane_borders.shows_borders(layout.pane_count() > 1)
            && self.pane_outer_borders
        {
            Borders::ALL
        } else {
            Borders::NONE
        };
        vec![PaneChromeInfo {
            id: zoomed_pane,
            rect: self.area,
            inner_rect: pane_inner_rect(self.area, borders),
            scrollbar_rect: None,
            borders,
            is_focused: true,
        }]
    }

    /// `(rows, cols)` for `pane_id` in `layout` (shown zoomed when `zoomed`),
    /// or `None` when the pane is not visible. A freshly spawned terminal is
    /// never on the alternate screen, so the scrollbar gutter is always
    /// reserved when enabled.
    pub fn pane_size(
        &self,
        layout: &TileLayout,
        zoomed: bool,
        pane_id: PaneId,
    ) -> Option<(u16, u16)> {
        let mut info = self
            .visible_panes(layout, zoomed)
            .into_iter()
            .find(|info| info.id == pane_id)?;
        info.content_layout(self.pane_scrollbars, false);
        let grid = shepr_core::geometry::PaneGeometry::with_cell(
            info.inner_rect.width,
            info.inner_rect.height,
            None,
        );
        Some((grid.rows(), grid.cols()))
    }

    /// `(rows, cols)` for the only pane of a new workspace.
    pub fn sole_pane_size(&self) -> (u16, u16) {
        let (layout, pane_id) = TileLayout::new();
        self.pane_size(&layout, false, pane_id)
            .unwrap_or((self.area.height.max(1), self.area.width.max(1)))
    }

    /// The PTY geometry of `pane_id`: its content grid (`pane_size`) with the
    /// pixel size of one cell, so the pane's first `TIOCSWINSZ` carries pixel
    /// dimensions. `None` when the pane is not visible.
    pub fn pane_spawn_geometry(
        &self,
        layout: &TileLayout,
        zoomed: bool,
        pane_id: PaneId,
        cell: Option<shepr_core::geometry::CellPx>,
    ) -> Option<shepr_core::geometry::PaneGeometry> {
        let (rows, cols) = self.pane_size(layout, zoomed, pane_id)?;
        Some(spawn_geometry(rows, cols, cell))
    }

    /// The PTY geometry of the only pane of a new workspace.
    pub fn sole_pane_spawn_geometry(
        &self,
        cell: Option<shepr_core::geometry::CellPx>,
    ) -> shepr_core::geometry::PaneGeometry {
        let (rows, cols) = self.sole_pane_size();
        spawn_geometry(rows, cols, cell)
    }
}

/// A PTY geometry of `rows` by `cols` whose cells measure `cell` pixels;
/// pixel-less when the cell size is unknown.
pub fn spawn_geometry(
    rows: u16,
    cols: u16,
    cell: Option<shepr_core::geometry::CellPx>,
) -> shepr_core::geometry::PaneGeometry {
    shepr_core::geometry::PaneGeometry::with_cell(cols, rows, cell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::layout::Direction;

    fn geometry(pane_borders: shepr_config::PaneBordersConfig, scrollbars: bool) -> PaneGeometry {
        PaneGeometry {
            area: Rect::new(0, 0, 100, 40),
            pane_borders,
            pane_gaps: false,
            pane_outer_borders: true,
            pane_scrollbars: scrollbars,
        }
    }

    #[test]
    fn chrome_adjacency_uses_wide_ends_at_coordinate_limits() {
        // Use literal rectangles: Rect::new clamps away overflowing ends.
        let rect = |x, y, width, height| shepr_core::geometry::Rect {
            x,
            y,
            width,
            height,
        };
        let pane = |rect| LayoutPaneInfo {
            id: PaneId::alloc(),
            rect,
            is_focused: false,
        };
        // A start at MAX cannot touch an edge mathematically beyond MAX.
        for (direction, from, to) in [
            (
                NavDirection::Right,
                rect(u16::MAX - 5, 0, 10, 10),
                rect(u16::MAX, 0, 1, 10),
            ),
            (
                NavDirection::Down,
                rect(0, u16::MAX - 5, 10, 10),
                rect(0, u16::MAX, 10, 1),
            ),
        ] {
            let from = pane(from);
            let to = pane(to);
            assert!(touching_neighbor(&from, &[to], direction).is_none());
        }
        // A cross-axis interval beginning at MAX still has real overlap.
        let from = pane(rect(0, u16::MAX, 10, 1));
        let to = pane(rect(10, u16::MAX, 10, 1));
        let panes = [from.clone(), to.clone()];
        assert_eq!(
            touching_neighbor(&from, &panes, NavDirection::Right).map(|pane| pane.id),
            Some(to.id)
        );
        let chrome =
            apply_pane_chrome(&panes, shepr_config::PaneBordersConfig::Always, false, true);
        assert!(!chrome[0].borders.contains(Borders::RIGHT));
    }

    #[test]
    fn empty_drawable_content_uses_the_same_minimum_grid_at_spawn_and_resize() {
        let geometry = PaneGeometry {
            area: Rect::new(0, 0, 0, 0),
            ..geometry(shepr_config::PaneBordersConfig::Always, true)
        };
        let (layout, root) = TileLayout::new();
        let mut pane = geometry.visible_panes(&layout, false).remove(0);
        assert_eq!(pane.content_layout(true, false), None);
        assert_eq!((pane.inner_rect.width, pane.inner_rect.height), (0, 0));
        let resized = shepr_core::geometry::PaneGeometry::with_cell(
            pane.inner_rect.width,
            pane.inner_rect.height,
            None,
        );
        assert_eq!(
            geometry.pane_size(&layout, false, root),
            Some((resized.rows(), resized.cols()))
        );
        // The shared pane grid minimum, not the empty drawable rect.
        assert_eq!((resized.rows(), resized.cols()), (2, 4));
    }

    #[test]
    fn content_layout_reserves_only_primary_wide_pane_gutters() {
        let (layout, _) = TileLayout::new();
        for width in [0, 1, 4, 5, 20] {
            for alternate in [false, true] {
                for scrollbars in [false, true] {
                    let geometry = PaneGeometry {
                        area: Rect::new(10, 3, width, 8),
                        ..geometry(shepr_config::PaneBordersConfig::Off, scrollbars)
                    };
                    let mut pane = geometry.visible_panes(&layout, false).remove(0);
                    let gutter = pane.content_layout(scrollbars, alternate);
                    let reserved = scrollbars && !alternate && width > 4;
                    assert_eq!(gutter.is_some(), reserved);
                    assert_eq!(pane.inner_rect.width, width - u16::from(reserved));
                    if let Some(gutter) = gutter {
                        assert_eq!(gutter, Rect::new(10 + width - 1, 3, 1, 8));
                    }
                }
            }
        }
    }

    #[test]
    fn spawn_geometry_pairs_the_content_grid_with_the_cell_pixel_size() {
        let geometry = geometry(shepr_config::PaneBordersConfig::Off, true);
        let cell = shepr_core::geometry::CellPx::new(9, 18);

        let sole = geometry.sole_pane_spawn_geometry(cell);
        assert_eq!((sole.rows(), sole.cols()), (40, 99));
        assert_eq!(sole.text_area_px(), Some((99 * 9, 40 * 18)));

        let (mut layout, root) = TileLayout::new();
        let right = layout
            .split_pane(
                root,
                Direction::Horizontal,
                shepr_core::layout::SplitRatio::EVEN,
            )
            .expect("split");
        let split = geometry
            .pane_spawn_geometry(&layout, false, right, cell)
            .expect("the new pane is visible");
        assert_eq!(
            (split.rows(), split.cols()),
            geometry.pane_size(&layout, false, right).expect("size")
        );
        assert_eq!(split.cell(), cell);

        // An unknown cell size leaves the pixel size out, not zero-sized.
        assert_eq!(geometry.sole_pane_spawn_geometry(None).cell(), None);
    }

    #[test]
    fn sole_pane_without_borders_keeps_only_the_scrollbar_gutter() {
        let geometry = geometry(shepr_config::PaneBordersConfig::Off, true);
        assert_eq!(geometry.sole_pane_size(), (40, 99));
        let geometry = PaneGeometry {
            pane_scrollbars: false,
            ..geometry
        };
        assert_eq!(geometry.sole_pane_size(), (40, 100));
    }

    #[test]
    fn split_pane_gets_its_own_half_not_the_split_target_size() {
        let geometry = geometry(shepr_config::PaneBordersConfig::Off, false);
        let (mut layout, root) = TileLayout::new();
        let right = layout
            .split_pane(
                root,
                Direction::Horizontal,
                shepr_core::layout::SplitRatio::EVEN,
            )
            .expect("test precondition");

        let (root_rows, root_cols) = geometry.pane_size(&layout, false, root).expect("root size");
        let (rows, cols) = geometry
            .pane_size(&layout, false, right)
            .expect("new pane size");

        assert_eq!(rows, 40);
        assert_eq!(root_rows, 40);
        assert_eq!(root_cols + cols, 100);
        assert!(cols < 100);
    }

    #[test]
    fn bordered_split_excludes_border_cells() {
        let borderless = geometry(shepr_config::PaneBordersConfig::Off, false);
        let bordered = geometry(shepr_config::PaneBordersConfig::Always, false);
        let (mut layout, root) = TileLayout::new();
        let below = layout
            .split_pane(
                root,
                Direction::Vertical,
                shepr_core::layout::SplitRatio::EVEN,
            )
            .expect("test precondition");

        let (plain_rows, plain_cols) = borderless.pane_size(&layout, false, below).expect("size");
        let (rows, cols) = bordered.pane_size(&layout, false, below).expect("size");

        assert!(rows < plain_rows);
        assert!(cols < plain_cols);
    }

    #[test]
    fn pane_outside_the_layout_has_no_size() {
        let geometry = geometry(shepr_config::PaneBordersConfig::Off, true);
        let (layout, _) = TileLayout::new();
        assert_eq!(geometry.pane_size(&layout, false, PaneId::alloc()), None);
    }

    #[test]
    fn zoomed_workspace_shows_only_the_focused_pane_over_the_whole_area() {
        let geometry = geometry(shepr_config::PaneBordersConfig::Always, false);
        let (mut layout, root) = TileLayout::new();
        let right = layout
            .split_pane(
                root,
                Direction::Horizontal,
                shepr_core::layout::SplitRatio::EVEN,
            )
            .expect("test precondition");
        layout.focus_pane(right);

        let panes = geometry.visible_panes(&layout, true);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].id, right);
        assert_eq!(panes[0].rect, geometry.area);
        assert!(panes[0].is_focused);
        assert_eq!(panes[0].borders, Borders::ALL);
        // Framed on every side: the whole area less one cell per edge.
        assert_eq!(geometry.pane_size(&layout, true, right), Some((38, 98)));
        assert_eq!(geometry.pane_size(&layout, true, root), None);
    }

    #[test]
    fn zoomed_pane_borders_follow_the_real_pane_count_and_outer_border_setting() {
        let (mut layout, root) = TileLayout::new();
        let chrome = |borders, outer| PaneGeometry {
            pane_outer_borders: outer,
            ..geometry(borders, false)
        };
        // A lone pane is never zoomed in practice, but the rule must still
        // agree with the tiled chrome: `Always` frames it, `Auto` does not.
        assert_eq!(
            chrome(shepr_config::PaneBordersConfig::Always, true).visible_panes(&layout, true)[0]
                .borders,
            Borders::ALL
        );
        assert_eq!(
            chrome(shepr_config::PaneBordersConfig::Auto, true).visible_panes(&layout, true)[0]
                .borders,
            Borders::NONE
        );
        layout
            .split_pane(
                root,
                Direction::Vertical,
                shepr_core::layout::SplitRatio::EVEN,
            )
            .expect("test precondition");
        assert_eq!(
            chrome(shepr_config::PaneBordersConfig::Auto, true).visible_panes(&layout, true)[0]
                .borders,
            Borders::ALL
        );
        assert_eq!(
            chrome(shepr_config::PaneBordersConfig::Always, false).visible_panes(&layout, true)[0]
                .borders,
            Borders::NONE
        );
        assert_eq!(
            chrome(shepr_config::PaneBordersConfig::Off, true).visible_panes(&layout, true)[0]
                .borders,
            Borders::NONE
        );
    }
}
