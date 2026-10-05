//! Pane chrome math on the layout model's cell rect: which pane edges are
//! shared with neighbors and the content rect left inside them. Pure cell
//! arithmetic with no drawing library, so the server's view code and the spawn
//! sizing that must agree with it share one implementation. Drawing crates
//! adapt these values at their boundary.

use crate::geometry::Rect;
use crate::layout::{NavDirection, PaneId, PaneInfo as LayoutPaneInfo, rect_distance_in_direction};
use crate::limits::PANE_MIN_COLS;

/// A pane always draws its top and left edges. These flags say that a neighbor
/// shares the pane's right or bottom edge, so this pane leaves that divider to
/// the neighbor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SharedPaneEdges {
    pub shares_right: bool,
    pub shares_bottom: bool,
}

/// The minimum terminal width that must remain after reserving a scrollbar
/// gutter.
pub const MIN_COLS_FOR_SCROLLBAR_GUTTER: u16 = PANE_MIN_COLS;

/// The part of `area` inside the pane edges: top and left are always inset;
/// right and bottom are inset when they are not shared. Saturates so a rect too
/// small for its borders ends up empty.
pub fn inner_rect(area: Rect, shared_edges: SharedPaneEdges) -> Rect {
    let mut inner = area;
    let right = area.x.saturating_add(area.width);
    let bottom = area.y.saturating_add(area.height);
    inner.x = inner.x.saturating_add(1).min(right);
    inner.width = inner.width.saturating_sub(1);
    inner.y = inner.y.saturating_add(1).min(bottom);
    inner.height = inner.height.saturating_sub(1);
    if !shared_edges.shares_right {
        inner.width = inner.width.saturating_sub(1);
    }
    if !shared_edges.shares_bottom {
        inner.height = inner.height.saturating_sub(1);
    }
    inner
}

/// The terminal content rect inside a pane's inner (border-less) rect: one
/// column is kept for the scrollbar gutter unless scrollbars are off, the pane
/// is too narrow, or the terminal is on the alternate screen.
pub fn content_rect(pane_inner: Rect, pane_scrollbars: bool, alternate_screen: bool) -> Rect {
    if !pane_scrollbars || pane_inner.width <= MIN_COLS_FOR_SCROLLBAR_GUTTER || alternate_screen {
        return pane_inner;
    }
    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

/// A pane's unchanged layout rect and the edges it shares with neighbors. The
/// layout rect is not shrunk for gaps. It carries no content geometry: the
/// screen mode decides the scrollbar gutter, so content exists only on the
/// [`PaneContent`] that [`PaneChrome::into_content`] produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneChrome {
    pub id: PaneId,
    pub rect: Rect,
    pub shared_edges: SharedPaneEdges,
    pub is_focused: bool,
}

impl PaneChrome {
    /// The rect inside the borders, before any scrollbar gutter.
    pub fn inner_rect(&self) -> Rect {
        inner_rect(self.rect, self.shared_edges)
    }

    /// Settle the content rect for a screen mode. Empty drawable rects stay
    /// empty; the PTY grid minimum belongs to the spawn sizing.
    pub fn into_content(self, scrollbars: bool, alternate_screen: bool) -> PaneContent {
        let inner = self.inner_rect();
        let content = content_rect(inner, scrollbars, alternate_screen);
        let scrollbar_gutter = (content != inner).then(|| {
            Rect::new(
                inner.x.saturating_add(inner.width.saturating_sub(1)),
                inner.y,
                1,
                inner.height,
            )
        });
        PaneContent {
            chrome: self,
            content,
            scrollbar_gutter,
        }
    }
}

/// A pane whose content rect is settled for its screen mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneContent {
    pub chrome: PaneChrome,
    /// Where the terminal's cells are drawn.
    pub content: Rect,
    /// The reserved scrollbar track; whether it draws depends on scrollback,
    /// which belongs to the consumer.
    pub scrollbar_gutter: Option<Rect>,
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

/// Find which right and bottom edges touch neighbors. Top and left edges always
/// belong to the pane; a right or bottom edge belongs to its neighbor only when
/// panes share dividers instead of drawing independent adjacent borders.
pub fn apply_pane_chrome(panes: &[LayoutPaneInfo], pane_gaps: bool) -> Vec<PaneChrome> {
    panes
        .iter()
        .map(|layout_info| {
            let shared_edges = SharedPaneEdges {
                shares_right: !pane_gaps
                    && touching_neighbor(layout_info, panes, NavDirection::Right).is_some(),
                shares_bottom: !pane_gaps
                    && touching_neighbor(layout_info, panes, NavDirection::Down).is_some(),
            };
            PaneChrome {
                id: layout_info.id,
                rect: layout_info.rect,
                shared_edges,
                is_focused: layout_info.is_focused,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Direction, SplitRatio, TileLayout};

    fn split(direction: Direction) -> (TileLayout, PaneId, PaneId) {
        let (mut layout, root) = TileLayout::new();
        let second = PaneId::alloc();
        assert!(layout.split_pane(root, direction, SplitRatio::EVEN, second));
        layout.focus_pane(root);
        (layout, root, second)
    }

    fn chrome_of(layout: &TileLayout, gaps: bool) -> Vec<PaneChrome> {
        apply_pane_chrome(&layout.panes(Rect::new(0, 0, 100, 20)), gaps)
    }

    fn find(chrome: &[PaneChrome], id: PaneId) -> &PaneChrome {
        chrome.iter().find(|pane| pane.id == id).expect("pane")
    }

    #[test]
    fn chrome_adjacency_uses_wide_ends_at_coordinate_limits() {
        // Use literal rectangles: Rect::new clamps away overflowing ends.
        let rect = |x, y, width, height| Rect {
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
        let chrome = apply_pane_chrome(&panes, false);
        assert!(chrome[0].shared_edges.shares_right);
    }

    #[test]
    fn inner_rect_always_takes_top_and_left_and_only_unshared_edges() {
        let area = Rect::new(4, 2, 10, 6);
        assert_eq!(
            inner_rect(area, SharedPaneEdges::default()),
            Rect::new(5, 3, 8, 4)
        );
        assert_eq!(
            inner_rect(
                area,
                SharedPaneEdges {
                    shares_right: true,
                    shares_bottom: false,
                }
            ),
            Rect::new(5, 3, 9, 4)
        );
        // A rect too small for its borders ends up empty, never wrapped.
        let tiny = inner_rect(Rect::new(0, 0, 1, 1), SharedPaneEdges::default());
        assert_eq!((tiny.width, tiny.height), (0, 0));
        let empty = inner_rect(
            Rect::new(u16::MAX, u16::MAX, 0, 0),
            SharedPaneEdges::default(),
        );
        assert_eq!((empty.width, empty.height), (0, 0));
    }

    #[test]
    fn content_layout_reserves_only_primary_wide_pane_gutters() {
        for width in [
            0,
            1,
            MIN_COLS_FOR_SCROLLBAR_GUTTER,
            MIN_COLS_FOR_SCROLLBAR_GUTTER + 1,
            MIN_COLS_FOR_SCROLLBAR_GUTTER + 2,
            MIN_COLS_FOR_SCROLLBAR_GUTTER + 3,
            20,
        ] {
            for alternate in [false, true] {
                for scrollbars in [false, true] {
                    let pane = PaneChrome {
                        id: PaneId::from_raw(1),
                        rect: Rect::new(10, 3, width, 8),
                        shared_edges: SharedPaneEdges::default(),
                        is_focused: true,
                    };
                    let inner = pane.inner_rect();
                    let settled = pane.into_content(scrollbars, alternate);
                    let reserved =
                        scrollbars && !alternate && inner.width > MIN_COLS_FOR_SCROLLBAR_GUTTER;
                    assert_eq!(settled.scrollbar_gutter.is_some(), reserved);
                    assert_eq!(settled.content.width, inner.width - u16::from(reserved));
                    if let Some(gutter) = settled.scrollbar_gutter {
                        assert_eq!(
                            gutter,
                            Rect::new(inner.x + inner.width - 1, inner.y, 1, inner.height)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn default_horizontal_split_uses_one_shared_divider_column() {
        let (layout, root, second) = split(Direction::Horizontal);
        let chrome = chrome_of(&layout, false);
        let left = find(&chrome, root);
        let right = find(&chrome, second);

        assert_eq!(left.rect.x + left.rect.width, right.rect.x);
        assert!(left.shared_edges.shares_right);
        assert!(!right.shared_edges.shares_right);
    }

    #[test]
    fn default_vertical_split_uses_one_shared_divider_row() {
        let (layout, root, second) = split(Direction::Vertical);
        let chrome = chrome_of(&layout, false);
        let top = find(&chrome, root);
        let bottom = find(&chrome, second);

        assert_eq!(top.rect.y + top.rect.height, bottom.rect.y);
        assert!(top.shared_edges.shares_bottom);
        assert!(!bottom.shared_edges.shares_bottom);
    }

    #[test]
    fn pane_gaps_keep_independent_bordered_panes() {
        let (layout, root, second) = split(Direction::Horizontal);
        let chrome = chrome_of(&layout, true);
        let left = find(&chrome, root);
        let right = find(&chrome, second);

        assert_eq!(left.rect.x + left.rect.width, right.rect.x);
        assert_eq!(left.shared_edges, SharedPaneEdges::default());
        assert_eq!(right.shared_edges, SharedPaneEdges::default());
    }

    #[test]
    fn lone_pane_is_framed_on_every_side() {
        let (layout, _) = TileLayout::new();
        assert_eq!(
            chrome_of(&layout, false)[0].shared_edges,
            SharedPaneEdges::default()
        );
    }
}
