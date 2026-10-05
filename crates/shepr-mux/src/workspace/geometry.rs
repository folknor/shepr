//! Size a pane's PTY is spawned at.
//!
//! A child reads its window size at startup (argv panes and agents often only
//! once), so a new pane must be spawned at the size view computation will give
//! it, not at some other pane's size. The view computes its pane rects through
//! the same `WorkspaceChrome::visible_panes` (BSP split, chrome, the zoomed case)
//! and the same `shepr_core::chrome` border and scrollbar gutter math.

use shepr_core::chrome::{PaneChrome, PaneContent, SharedPaneEdges, apply_pane_chrome};
use shepr_core::geometry::{CellPx, GridSize, PaneGeometry, Rect};
use shepr_core::layout::{PaneId, TileLayout};

/// The area a workspace's panes are laid out in and the pixel size of one cell
/// there: everything besides the pane tree that decides what size a pane's PTY
/// gets, and so what a pane spawned into that workspace starts at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnGeometry {
    pub area: Rect,
    pub cell: Option<CellPx>,
}

impl SpawnGeometry {
    pub fn for_grid(grid: GridSize, cell: Option<CellPx>) -> Self {
        Self {
            area: grid.rect(),
            cell,
        }
    }

    /// The pixel size of one cell, `None` when the host never reported one.
    pub fn cell_px(&self) -> Option<CellPx> {
        self.cell
    }
}

/// Everything besides the layout tree that decides a pane's content size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceChrome {
    /// Area the workspace's panes are laid out in.
    pub area: Rect,
    pub pane_gaps: bool,
    pub pane_scrollbars: bool,
}

impl WorkspaceChrome {
    /// The sole pane shown by zoom, shared by ID and geometry projections.
    pub fn zoomed_pane(layout: &TileLayout, zoomed: bool) -> Option<PaneId> {
        zoomed.then(|| layout.focused())
    }

    /// Restore every pane: hidden panes use tiled geometry, while the visible
    /// zoomed pane uses its surface geometry. Resumes start on the primary screen.
    pub fn resume_panes(&self, layout: &TileLayout, zoomed: bool) -> Vec<PaneContent> {
        let mut panes = self.visible_panes(layout, false);
        if zoomed {
            for visible in self.visible_panes(layout, true) {
                if let Some(pane) = panes.iter_mut().find(|pane| pane.id == visible.id) {
                    *pane = visible;
                }
            }
        }
        panes
            .into_iter()
            .map(|pane| pane.into_content(self.pane_scrollbars, false))
            .collect()
    }

    /// The visible panes of a workspace with their chrome applied: layout rects
    /// and shared-edge facts. Call `PaneChrome::into_content` to settle content
    /// using the pane's screen mode; scroll metrics then select whether its
    /// track draws.
    ///
    /// A zoomed workspace shows only its focused pane, filling `area`, framed
    /// on all sides.
    /// View computation, background resizing and spawn sizing all go through
    /// here, so the zoomed rule exists once.
    pub fn visible_panes(&self, layout: &TileLayout, zoomed: bool) -> Vec<PaneChrome> {
        let Some(zoomed_pane) = Self::zoomed_pane(layout, zoomed) else {
            return apply_pane_chrome(&layout.panes(self.area), self.pane_gaps);
        };
        vec![PaneChrome {
            id: zoomed_pane,
            rect: self.area,
            shared_edges: SharedPaneEdges::default(),
            is_focused: true,
        }]
    }

    /// The content grid for `pane_id` in `layout` (shown zoomed when `zoomed`),
    /// or `None` when the pane is not visible. A freshly spawned terminal is
    /// never on the alternate screen, so the scrollbar gutter is always
    /// reserved when enabled.
    pub fn pane_size(
        &self,
        layout: &TileLayout,
        zoomed: bool,
        pane_id: PaneId,
    ) -> Option<GridSize> {
        let content = self
            .visible_panes(layout, zoomed)
            .into_iter()
            .find(|info| info.id == pane_id)?
            .into_content(self.pane_scrollbars, false)
            .content;
        Some(PaneGeometry::cells_only(content.width, content.height).grid())
    }

    /// The content grid for the only pane of a new workspace.
    pub fn sole_pane_size(&self) -> GridSize {
        let (layout, pane_id) = TileLayout::new();
        self.pane_size(&layout, false, pane_id)
            .unwrap_or_else(|| GridSize::clamped(self.area.width, self.area.height))
    }

    /// The PTY geometry of `pane_id`: its content grid (`pane_size`) with the
    /// pixel size of one cell, so the pane's first `TIOCSWINSZ` carries pixel
    /// dimensions. `None` when the pane is not visible.
    pub fn pane_spawn_geometry(
        &self,
        layout: &TileLayout,
        zoomed: bool,
        pane_id: PaneId,
        cell: Option<CellPx>,
    ) -> Option<PaneGeometry> {
        let grid = self.pane_size(layout, zoomed, pane_id)?;
        Some(spawn_geometry(grid, cell))
    }

    /// The PTY geometry of the only pane of a new workspace.
    pub fn sole_pane_spawn_geometry(&self, cell: Option<CellPx>) -> PaneGeometry {
        spawn_geometry(self.sole_pane_size(), cell)
    }
}

/// A PTY geometry of `grid` whose cells measure `cell` pixels; pixel-less when
/// the cell size is unknown.
pub fn spawn_geometry(grid: GridSize, cell: Option<CellPx>) -> PaneGeometry {
    PaneGeometry::with_cell(grid.cols(), grid.rows(), cell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::chrome::inner_rect;
    use shepr_core::layout::Direction;

    fn geometry(scrollbars: bool) -> WorkspaceChrome {
        WorkspaceChrome {
            area: Rect::new(0, 0, 100, 40),
            pane_gaps: false,
            pane_scrollbars: scrollbars,
        }
    }

    fn grid(rows: u16, cols: u16) -> GridSize {
        GridSize::new(cols, rows).expect("test grid is nonzero")
    }

    #[test]
    fn empty_drawable_content_uses_the_same_minimum_grid_at_spawn_and_resize() {
        let geometry = WorkspaceChrome {
            area: Rect::new(0, 0, 0, 0),
            ..geometry(true)
        };
        let (layout, root) = TileLayout::new();
        let pane = geometry.visible_panes(&layout, false).remove(0);
        let settled = pane.into_content(true, false);
        assert_eq!(settled.scrollbar_gutter, None);
        assert_eq!((settled.content.width, settled.content.height), (0, 0));
        let resized = PaneGeometry::with_cell(settled.content.width, settled.content.height, None);
        assert_eq!(
            geometry.pane_size(&layout, false, root),
            Some(resized.grid())
        );
        // The shared pane grid minimum, not the empty drawable rect.
        assert_eq!((resized.rows(), resized.cols()), (2, 4));
    }

    #[test]
    fn spawn_geometry_pairs_the_content_grid_with_the_cell_pixel_size() {
        let geometry = geometry(true);
        let cell = CellPx::new(9, 18);

        let sole = geometry.sole_pane_spawn_geometry(cell);
        assert_eq!((sole.rows(), sole.cols()), (38, 97));
        let extent = sole.pixel_extent().expect("cell known");
        assert_eq!(
            (extent.width().get(), extent.height().get()),
            (97 * 9, 38 * 18)
        );

        let (mut layout, root) = TileLayout::new();
        let right = PaneId::alloc();
        assert!(layout.split_pane(
            root,
            Direction::Horizontal,
            shepr_core::layout::SplitRatio::EVEN,
            right,
        ));
        let split = geometry
            .pane_spawn_geometry(&layout, false, right, cell)
            .expect("the new pane is visible");
        assert_eq!(
            split.grid(),
            geometry.pane_size(&layout, false, right).expect("size")
        );
        assert_eq!(split.cell(), cell);

        // An unknown cell size leaves the pixel size out, not zero-sized.
        assert_eq!(geometry.sole_pane_spawn_geometry(None).cell(), None);
    }

    #[test]
    fn sole_pane_is_framed_and_keeps_the_scrollbar_gutter() {
        let geometry = geometry(true);
        assert_eq!(geometry.sole_pane_size(), grid(38, 97));
        let geometry = WorkspaceChrome {
            pane_scrollbars: false,
            ..geometry
        };
        assert_eq!(geometry.sole_pane_size(), grid(38, 98));
    }

    #[test]
    fn split_pane_gets_its_own_half_not_the_split_target_size() {
        let geometry = geometry(false);
        let (mut layout, root) = TileLayout::new();
        let right = PaneId::alloc();
        assert!(layout.split_pane(
            root,
            Direction::Horizontal,
            shepr_core::layout::SplitRatio::EVEN,
            right,
        ));

        let root_size = geometry.pane_size(&layout, false, root).expect("root size");
        let size = geometry
            .pane_size(&layout, false, right)
            .expect("new pane size");

        // Full height less the top and bottom border rows; the width less the
        // outer left and right columns and the one shared divider.
        assert_eq!(size.rows(), 38);
        assert_eq!(root_size.rows(), 38);
        assert_eq!(root_size.cols() + size.cols(), 97);
        assert!(size.cols() < 100);
    }

    #[test]
    fn split_panes_exclude_border_cells() {
        let geometry = geometry(false);
        let (mut layout, root) = TileLayout::new();
        let below = PaneId::alloc();
        assert!(layout.split_pane(
            root,
            Direction::Vertical,
            shepr_core::layout::SplitRatio::EVEN,
            below,
        ));

        // The upper pane leaves its bottom side to the shared divider.
        assert_eq!(geometry.pane_size(&layout, false, root), Some(grid(19, 98)));
        assert_eq!(
            geometry.pane_size(&layout, false, below),
            Some(grid(18, 98))
        );
    }

    #[test]
    fn pane_outside_the_layout_has_no_size() {
        let geometry = geometry(true);
        let (layout, _) = TileLayout::new();
        assert_eq!(geometry.pane_size(&layout, false, PaneId::alloc()), None);
    }

    #[test]
    fn zoomed_workspace_shows_only_the_focused_pane_over_the_whole_area() {
        let geometry = geometry(false);
        let (mut layout, root) = TileLayout::new();
        let right = PaneId::alloc();
        assert!(layout.split_pane(
            root,
            Direction::Horizontal,
            shepr_core::layout::SplitRatio::EVEN,
            right,
        ));
        layout.focus_pane(right);

        let panes = geometry.visible_panes(&layout, true);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].id, right);
        assert_eq!(panes[0].rect, geometry.area);
        assert!(panes[0].is_focused);
        assert_eq!(panes[0].shared_edges, SharedPaneEdges::default());
        // Framed on every side: the whole area less one cell per edge.
        assert_eq!(
            inner_rect(panes[0].rect, panes[0].shared_edges),
            Rect::new(1, 1, 98, 38)
        );
        assert_eq!(geometry.pane_size(&layout, true, right), Some(grid(38, 98)));
        assert_eq!(geometry.pane_size(&layout, true, root), None);
    }

    #[test]
    fn zoomed_pane_is_framed_whatever_the_pane_count() {
        let (mut layout, root) = TileLayout::new();
        let chrome = geometry(false);
        assert_eq!(
            chrome.visible_panes(&layout, true)[0].shared_edges,
            SharedPaneEdges::default()
        );
        assert!(layout.split_pane(
            root,
            Direction::Vertical,
            shepr_core::layout::SplitRatio::EVEN,
            PaneId::alloc(),
        ));
        assert_eq!(
            chrome.visible_panes(&layout, true)[0].shared_edges,
            SharedPaneEdges::default()
        );
    }
}
