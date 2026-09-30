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

use shepr_core::layout::{PaneId, PaneInfo as LayoutPaneInfo, TileLayout};

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
#[derive(Clone)]
pub struct PaneChromeInfo {
    pub id: PaneId,
    pub rect: Rect,
    pub inner_rect: Rect,
    pub scrollbar_rect: Option<Rect>,
    pub borders: Borders,
    pub is_focused: bool,
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

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    a_start < b_start.saturating_add(b_len) && b_start < a_start.saturating_add(a_len)
}

fn pane_to_right<'a>(
    info: &LayoutPaneInfo,
    panes: &'a [LayoutPaneInfo],
) -> Option<&'a LayoutPaneInfo> {
    let right = info.rect.x.saturating_add(info.rect.width);
    panes.iter().find(|other| {
        other.id != info.id
            && other.rect.x == right
            && ranges_overlap(
                info.rect.y,
                info.rect.height,
                other.rect.y,
                other.rect.height,
            )
    })
}

fn pane_below<'a>(
    info: &LayoutPaneInfo,
    panes: &'a [LayoutPaneInfo],
) -> Option<&'a LayoutPaneInfo> {
    let bottom = info.rect.y.saturating_add(info.rect.height);
    panes.iter().find(|other| {
        other.id != info.id
            && other.rect.y == bottom
            && ranges_overlap(info.rect.x, info.rect.width, other.rect.x, other.rect.width)
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
        .map(|info| info.rect.x.saturating_add(info.rect.width))
        .max()
        .unwrap_or(0);
    let outer_bottom = panes
        .iter()
        .map(|info| info.rect.y.saturating_add(info.rect.height))
        .max()
        .unwrap_or(0);
    panes
        .iter()
        .cloned()
        .map(|layout_info| {
            let right_neighbor = multi_pane
                .then(|| pane_to_right(&layout_info, panes))
                .flatten();
            let below_neighbor = multi_pane
                .then(|| pane_below(&layout_info, panes))
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
                    if info.rect.x.saturating_add(info.rect.width) == outer_right {
                        borders.remove(Borders::RIGHT);
                    }
                    if info.rect.y.saturating_add(info.rect.height) == outer_bottom {
                        borders.remove(Borders::BOTTOM);
                    }
                }
                borders
            };
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
    /// The visible panes of a workspace with their chrome applied: outer rect and
    /// borders. `inner_rect` and `scrollbar_rect` are not settled here;
    /// callers derive the content rect from `rect` and `borders`.
    ///
    /// A zoomed workspace shows only its focused pane, filling `area`. Every
    /// edge of that pane is an outer edge, so it is framed on all sides exactly
    /// when borders show for the workspace's real pane count and outer borders
    /// are on.
    /// View computation, background resizing and spawn sizing all go through
    /// here, so the zoomed rule exists once.
    pub fn visible_panes(&self, layout: &TileLayout, zoomed: bool) -> Vec<PaneChromeInfo> {
        if !zoomed {
            return apply_pane_chrome(
                &layout.panes(layout_rect(self.area)),
                self.pane_borders,
                self.pane_gaps,
                self.pane_outer_borders,
            );
        }
        let borders = if self.pane_borders.shows_borders(layout.pane_count() > 1)
            && self.pane_outer_borders
        {
            Borders::ALL
        } else {
            Borders::NONE
        };
        vec![PaneChromeInfo {
            id: layout.focused(),
            rect: self.area,
            inner_rect: self.area,
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
        let info = self
            .visible_panes(layout, zoomed)
            .into_iter()
            .find(|info| info.id == pane_id)?;
        let pane_inner = pane_inner_rect(info.rect, info.borders);
        let content = terminal_content_rect(pane_inner, self.pane_scrollbars, false);
        Some((content.height.max(1), content.width.max(1)))
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
    shepr_core::geometry::PaneGeometry::new(
        cols,
        rows,
        cell.map_or(0, |cell| cell.width.get()),
        cell.map_or(0, |cell| cell.height.get()),
    )
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
    fn spawn_geometry_pairs_the_content_grid_with_the_cell_pixel_size() {
        let geometry = geometry(shepr_config::PaneBordersConfig::Off, true);
        let cell = shepr_core::geometry::CellPx::new(9, 18);

        let sole = geometry.sole_pane_spawn_geometry(cell);
        assert_eq!((sole.rows(), sole.cols()), (40, 99));
        assert_eq!(sole.text_area_px(), Some((99 * 9, 40 * 18)));

        let (mut layout, root) = TileLayout::new();
        let right = layout
            .split_pane(root, Direction::Horizontal, 0.5)
            .expect("split");
        let split = geometry
            .pane_spawn_geometry(&layout, false, right, cell)
            .expect("the new pane is visible");
        assert_eq!(
            (split.rows(), split.cols()),
            geometry.pane_size(&layout, false, right).expect("size")
        );
        assert_eq!(split.cell, cell);

        // An unknown cell size leaves the pixel size out, not zero-sized.
        assert_eq!(geometry.sole_pane_spawn_geometry(None).cell, None);
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
            .split_pane(root, Direction::Horizontal, 0.5)
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
            .split_pane(root, Direction::Vertical, 0.5)
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
            .split_pane(root, Direction::Horizontal, 0.5)
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
            .split_pane(root, Direction::Vertical, 0.5)
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
