//! Pane chrome math on the layout model's cell rect: which borders a pane
//! draws, the gaps between panes, and the content rect left inside them. Pure
//! cell arithmetic with no drawing library, so the server's view code and the
//! spawn sizing that must agree with it share one implementation. Drawing
//! crates adapt these values at their boundary.

use std::ops::BitOr;

use crate::geometry::Rect;
use crate::layout::{NavDirection, PaneId, PaneInfo as LayoutPaneInfo, rect_distance_in_direction};

/// A pane's border sides as a small bitset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Borders(u8);

impl Borders {
    pub const NONE: Self = Self(0);
    pub const TOP: Self = Self(1);
    pub const RIGHT: Self = Self(1 << 1);
    pub const BOTTOM: Self = Self(1 << 2);
    pub const LEFT: Self = Self(1 << 3);
    pub const ALL: Self = Self(0b1111);

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every side of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl BitOr for Borders {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The part of `area` inside `borders`: one cell off each bordered side,
/// saturating, so a rect too small for its borders ends up empty.
pub fn inner_rect(area: Rect, borders: Borders) -> Rect {
    let mut inner = area;
    let right = area.x.saturating_add(area.width);
    let bottom = area.y.saturating_add(area.height);
    if borders.contains(Borders::LEFT) {
        inner.x = inner.x.saturating_add(1).min(right);
        inner.width = inner.width.saturating_sub(1);
    }
    if borders.contains(Borders::TOP) {
        inner.y = inner.y.saturating_add(1).min(bottom);
        inner.height = inner.height.saturating_sub(1);
    }
    if borders.contains(Borders::RIGHT) {
        inner.width = inner.width.saturating_sub(1);
    }
    if borders.contains(Borders::BOTTOM) {
        inner.height = inner.height.saturating_sub(1);
    }
    inner
}

/// The terminal content rect inside a pane's inner (border-less) rect: one
/// column is kept for the scrollbar gutter unless scrollbars are off, the pane
/// is too narrow, or the terminal is on the alternate screen.
pub fn content_rect(pane_inner: Rect, pane_scrollbars: bool, alternate_screen: bool) -> Rect {
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

/// A pane's layout position with its chrome applied: the outer rect (gaps
/// already taken off) and the borders it draws. It carries no content
/// geometry: the screen mode decides the scrollbar gutter, so content exists
/// only on the [`PaneContent`] that [`PaneChrome::into_content`] produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneChrome {
    pub id: PaneId,
    pub rect: Rect,
    pub borders: Borders,
    pub is_focused: bool,
}

impl PaneChrome {
    /// The rect inside the borders, before any scrollbar gutter.
    pub fn inner_rect(&self) -> Rect {
        inner_rect(self.rect, self.borders)
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

/// Apply the pane chrome settings to a layout's pane rects. Every pane draws
/// its full border box; with no gaps, a side shared with a neighbouring pane
/// is left to that neighbour so the two share one divider line.
pub fn apply_pane_chrome(panes: &[LayoutPaneInfo], pane_gaps: bool) -> Vec<PaneChrome> {
    panes
        .iter()
        .map(|layout_info| {
            let mut borders = Borders::ALL;
            if !pane_gaps {
                if touching_neighbor(layout_info, panes, NavDirection::Right).is_some() {
                    borders.remove(Borders::RIGHT);
                }
                if touching_neighbor(layout_info, panes, NavDirection::Down).is_some() {
                    borders.remove(Borders::BOTTOM);
                }
            }
            PaneChrome {
                id: layout_info.id,
                rect: layout_info.rect,
                borders,
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
        assert!(!chrome[0].borders.contains(Borders::RIGHT));
    }

    #[test]
    fn inner_rect_takes_one_cell_off_each_bordered_side() {
        let area = Rect::new(4, 2, 10, 6);
        assert_eq!(inner_rect(area, Borders::NONE), area);
        assert_eq!(inner_rect(area, Borders::ALL), Rect::new(5, 3, 8, 4));
        assert_eq!(
            inner_rect(area, Borders::LEFT | Borders::BOTTOM),
            Rect::new(5, 2, 9, 5)
        );
        // A rect too small for its borders ends up empty, never wrapped.
        let tiny = inner_rect(Rect::new(0, 0, 1, 1), Borders::ALL);
        assert_eq!((tiny.width, tiny.height), (0, 0));
        let empty = inner_rect(Rect::new(u16::MAX, u16::MAX, 0, 0), Borders::ALL);
        assert_eq!((empty.width, empty.height), (0, 0));
    }

    #[test]
    fn content_layout_reserves_only_primary_wide_pane_gutters() {
        for width in [0, 1, 4, 5, 20] {
            for alternate in [false, true] {
                for scrollbars in [false, true] {
                    let pane = PaneChrome {
                        id: PaneId::from_raw(1),
                        rect: Rect::new(10, 3, width, 8),
                        borders: Borders::NONE,
                        is_focused: true,
                    };
                    let settled = pane.into_content(scrollbars, alternate);
                    let reserved = scrollbars && !alternate && width > 4;
                    assert_eq!(settled.scrollbar_gutter.is_some(), reserved);
                    assert_eq!(settled.content.width, width - u16::from(reserved));
                    if let Some(gutter) = settled.scrollbar_gutter {
                        assert_eq!(gutter, Rect::new(10 + width - 1, 3, 1, 8));
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
        assert!(!left.borders.contains(Borders::RIGHT));
        assert!(right.borders.contains(Borders::LEFT));
    }

    #[test]
    fn default_vertical_split_uses_one_shared_divider_row() {
        let (layout, root, second) = split(Direction::Vertical);
        let chrome = chrome_of(&layout, false);
        let top = find(&chrome, root);
        let bottom = find(&chrome, second);

        assert_eq!(top.rect.y + top.rect.height, bottom.rect.y);
        assert!(!top.borders.contains(Borders::BOTTOM));
        assert!(bottom.borders.contains(Borders::TOP));
    }

    #[test]
    fn pane_gaps_keep_independent_bordered_panes() {
        let (layout, root, second) = split(Direction::Horizontal);
        let chrome = chrome_of(&layout, true);
        let left = find(&chrome, root);
        let right = find(&chrome, second);

        assert_eq!(left.rect.x + left.rect.width, right.rect.x);
        assert_eq!(left.borders, Borders::ALL);
        assert_eq!(right.borders, Borders::ALL);
    }

    #[test]
    fn lone_pane_is_framed_on_every_side() {
        let (layout, _) = TileLayout::new();
        assert_eq!(chrome_of(&layout, false)[0].borders, Borders::ALL);
    }
}
