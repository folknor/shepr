//! Size a pane's PTY is spawned at.
//!
//! A child reads its window size at startup (argv panes and agents often only
//! once), so a new pane must be spawned at the size view computation will give
//! it, not at some other pane's size. The view computes its pane rects through
//! the same `PaneGeometry::tab_panes` (BSP split, chrome, the zoomed case), and
//! the same `ui::pane_inner_rect` and scrollbar gutter.

use ratatui::{layout::Rect, widgets::Borders};

use crate::layout::{PaneId, TileLayout};
use crate::ui::PaneChromeInfo as PaneInfo;

/// Everything besides the layout tree that decides a pane's content size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneGeometry {
    /// Area the tab's panes are laid out in.
    pub area: Rect,
    pub pane_borders: crate::config::PaneBordersConfig,
    pub pane_gaps: bool,
    pub pane_outer_borders: bool,
    pub pane_scrollbars: bool,
}

/// The terminal content rect inside a pane's inner (border-less) rect: one
/// column is kept for the scrollbar gutter unless scrollbars are off, the pane
/// is too narrow, or the terminal is on the alternate screen.
pub(crate) fn terminal_content_rect(
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
    /// The visible panes of a tab with their chrome applied: outer rect and
    /// borders. `inner_rect` and `scrollbar_rect` are not settled here;
    /// callers derive the content rect from `rect` and `borders`.
    ///
    /// A zoomed tab shows only its focused pane, filling `area`. Every edge of
    /// that pane is an outer edge, so it is framed on all sides exactly when
    /// borders show for the tab's real pane count and outer borders are on.
    /// View computation, background resizing and spawn sizing all go through
    /// here, so the zoomed rule exists once.
    pub(crate) fn tab_panes(&self, layout: &TileLayout, zoomed: bool) -> Vec<PaneInfo> {
        if !zoomed {
            return crate::ui::apply_pane_chrome(
                &layout.panes(self.area),
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
        vec![PaneInfo {
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
    pub(crate) fn pane_size(
        &self,
        layout: &TileLayout,
        zoomed: bool,
        pane_id: PaneId,
    ) -> Option<(u16, u16)> {
        let info = self
            .tab_panes(layout, zoomed)
            .into_iter()
            .find(|info| info.id == pane_id)?;
        let pane_inner = crate::ui::pane_inner_rect(info.rect, info.borders);
        let content = terminal_content_rect(pane_inner, self.pane_scrollbars, false);
        Some((content.height.max(1), content.width.max(1)))
    }

    /// `(rows, cols)` for the only pane of a new tab or workspace.
    pub(crate) fn sole_pane_size(&self) -> (u16, u16) {
        let (layout, pane_id) = TileLayout::new();
        self.pane_size(&layout, false, pane_id)
            .unwrap_or((self.area.height.max(1), self.area.width.max(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Direction;

    fn geometry(pane_borders: crate::config::PaneBordersConfig, scrollbars: bool) -> PaneGeometry {
        PaneGeometry {
            area: Rect::new(0, 0, 100, 40),
            pane_borders,
            pane_gaps: false,
            pane_outer_borders: true,
            pane_scrollbars: scrollbars,
        }
    }

    #[test]
    fn sole_pane_without_borders_keeps_only_the_scrollbar_gutter() {
        let geometry = geometry(crate::config::PaneBordersConfig::Off, true);
        assert_eq!(geometry.sole_pane_size(), (40, 99));
        let geometry = PaneGeometry {
            pane_scrollbars: false,
            ..geometry
        };
        assert_eq!(geometry.sole_pane_size(), (40, 100));
    }

    #[test]
    fn split_pane_gets_its_own_half_not_the_split_target_size() {
        let geometry = geometry(crate::config::PaneBordersConfig::Off, false);
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
        let borderless = geometry(crate::config::PaneBordersConfig::Off, false);
        let bordered = geometry(crate::config::PaneBordersConfig::Always, false);
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
        let geometry = geometry(crate::config::PaneBordersConfig::Off, true);
        let (layout, _) = TileLayout::new();
        assert_eq!(geometry.pane_size(&layout, false, PaneId::alloc()), None);
    }

    #[test]
    fn zoomed_tab_shows_only_the_focused_pane_over_the_whole_area() {
        let geometry = geometry(crate::config::PaneBordersConfig::Always, false);
        let (mut layout, root) = TileLayout::new();
        let right = layout
            .split_pane(root, Direction::Horizontal, 0.5)
            .expect("test precondition");
        layout.focus_pane(right);

        let panes = geometry.tab_panes(&layout, true);
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
            chrome(crate::config::PaneBordersConfig::Always, true).tab_panes(&layout, true)[0]
                .borders,
            Borders::ALL
        );
        assert_eq!(
            chrome(crate::config::PaneBordersConfig::Auto, true).tab_panes(&layout, true)[0]
                .borders,
            Borders::NONE
        );
        layout
            .split_pane(root, Direction::Vertical, 0.5)
            .expect("test precondition");
        assert_eq!(
            chrome(crate::config::PaneBordersConfig::Auto, true).tab_panes(&layout, true)[0]
                .borders,
            Borders::ALL
        );
        assert_eq!(
            chrome(crate::config::PaneBordersConfig::Always, false).tab_panes(&layout, true)[0]
                .borders,
            Borders::NONE
        );
        assert_eq!(
            chrome(crate::config::PaneBordersConfig::Off, true).tab_panes(&layout, true)[0].borders,
            Borders::NONE
        );
    }
}
