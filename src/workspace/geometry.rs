//! Size a pane's PTY is spawned at.
//!
//! A child reads its window size at startup (argv panes and agents often only
//! once), so a new pane must be spawned at the size view computation will give
//! it, not at some other pane's size. This mirrors the view's rules: the same
//! BSP split (`TileLayout::panes`), the same chrome (`ui::apply_pane_chrome`,
//! `ui::pane_inner_rect`) and the same scrollbar gutter.

use ratatui::layout::Rect;

use crate::layout::{PaneId, TileLayout};

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
    /// `(rows, cols)` for `pane_id` in `layout`, or `None` when the pane is not
    /// in the layout. A freshly spawned terminal is never on the alternate
    /// screen, so the scrollbar gutter is always reserved when enabled.
    pub(crate) fn pane_size(&self, layout: &TileLayout, pane_id: PaneId) -> Option<(u16, u16)> {
        let info = crate::ui::apply_pane_chrome(
            &layout.panes(self.area),
            self.pane_borders,
            self.pane_gaps,
            self.pane_outer_borders,
        )
        .into_iter()
        .find(|info| info.id == pane_id)?;
        let pane_inner = crate::ui::pane_inner_rect(info.rect, info.borders);
        let content = terminal_content_rect(pane_inner, self.pane_scrollbars, false);
        Some((content.height.max(1), content.width.max(1)))
    }

    /// `(rows, cols)` for the only pane of a new tab or workspace.
    pub(crate) fn sole_pane_size(&self) -> (u16, u16) {
        let (layout, pane_id) = TileLayout::new();
        self.pane_size(&layout, pane_id)
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

        let (root_rows, root_cols) = geometry.pane_size(&layout, root).expect("root size");
        let (rows, cols) = geometry.pane_size(&layout, right).expect("new pane size");

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

        let (plain_rows, plain_cols) = borderless.pane_size(&layout, below).expect("size");
        let (rows, cols) = bordered.pane_size(&layout, below).expect("size");

        assert!(rows < plain_rows);
        assert!(cols < plain_cols);
    }

    #[test]
    fn pane_outside_the_layout_has_no_size() {
        let geometry = geometry(crate::config::PaneBordersConfig::Off, true);
        let (layout, _) = TileLayout::new();
        assert_eq!(geometry.pane_size(&layout, PaneId::alloc()), None);
    }
}
