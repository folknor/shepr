//! How one pane looks on a client's surface. The chrome math lives in
//! `shepr_core::chrome` on the layout model's cell rect; `PaneSurface` settles
//! it for the pane's screen mode and scrollback and adds the presentation
//! decisions made per pane: the reserved scrollbar gutter, whether the track
//! draws, its cells and the cursor. The full render and the server's retained
//! patch diff both consume it, so neither re-derives any of them.

use ratatui::layout::Rect;
use shepr_core::chrome::{PaneChrome, PaneContent, SharedPaneEdges};
use shepr_core::layout::PaneId;
use shepr_mux::pane::{PaneRuntime, ScrollMetrics};
use shepr_protocol::{CellData, CursorState, PaneSurfacePane, SurfaceRect};
use shepr_surface::ratatui_conversion::surface_rect;

use super::scrollbar::visit_scrollbar_track;
use crate::app::AppState;

/// The layout model's rect as the one ratatui draws into. Both are plain
/// cell coordinates, so the conversion copies the fields. These are free
/// functions because neither type is local to any shepr crate that sees both,
/// so no `From` impl between them can exist.
pub(crate) fn ratatui_rect(rect: shepr_core::geometry::Rect) -> Rect {
    Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// A ratatui rect as the layout model's rect; the inverse of `ratatui_rect`.
pub(crate) fn core_rect(rect: Rect) -> shepr_core::geometry::Rect {
    shepr_core::geometry::Rect::new(rect.x, rect.y, rect.width, rect.height)
}

/// One pane's scrollbar column as a retained patch rewrites it.
pub(crate) struct ScrollbarPaint {
    /// The column to rewrite.
    pub(crate) rect: SurfaceRect,
    metrics: Option<ScrollMetrics>,
    focused: bool,
}

impl ScrollbarPaint {
    /// Hands `visit` each cell of the column with its row from the top.
    pub(crate) fn visit(&self, visit: impl FnMut(u16, CellData)) {
        visit_scrollbar_track(self.rect.height, self.metrics, self.focused, visit);
    }
}

/// A pane with its content settled for its screen mode, in ratatui terms:
/// what the surface draws and the retained-render path compares against.
/// It stays ratatui-typed for geometry because the server draws into ratatui
/// buffers; its shared-edge facts come directly from the core chrome model.
#[derive(Clone)]
pub(crate) struct PaneSurface {
    pub(crate) id: PaneId,
    pub(crate) rect: Rect,
    /// Where the terminal's cells are drawn.
    pub(crate) inner_rect: Rect,
    /// The track column the pane reserves for a scrollbar, whether or not it
    /// has scrollback to show.
    pub(crate) scrollbar_gutter: Option<Rect>,
    /// The scrollbar track, present only when it draws.
    pub(crate) scrollbar_rect: Option<Rect>,
    pub(crate) shared_edges: SharedPaneEdges,
    pub(crate) is_focused: bool,
}

impl PaneSurface {
    /// The pane as drawn in a screen mode: `chrome` settled for `scrollbars`
    /// and `alternate_screen`. `metrics` is read only when the pane reserved a
    /// track, and decides whether the track draws.
    pub(crate) fn settle(
        chrome: PaneChrome,
        scrollbars: bool,
        alternate_screen: bool,
        metrics: impl FnOnce() -> Option<ScrollMetrics>,
    ) -> Self {
        let mut surface = Self::from_content(&chrome.into_content(scrollbars, alternate_screen));
        if surface.scrollbar_gutter.is_some() {
            surface.set_scroll(metrics());
        }
        surface
    }

    fn from_content(content: &PaneContent) -> Self {
        Self {
            id: content.chrome.id,
            rect: ratatui_rect(content.chrome.rect),
            inner_rect: ratatui_rect(content.content),
            scrollbar_gutter: content.scrollbar_gutter.map(ratatui_rect),
            scrollbar_rect: None,
            shared_edges: content.chrome.shared_edges,
            is_focused: content.chrome.is_focused,
        }
    }

    /// Whether a reserved track has scrollback to display.
    fn scrollbar_visible(metrics: ScrollMetrics) -> bool {
        metrics.max_offset_from_bottom > 0
    }

    /// The same pane with its scrollbar decided by `metrics`.
    pub(crate) fn with_scroll(mut self, metrics: Option<ScrollMetrics>) -> Self {
        self.set_scroll(metrics);
        self
    }

    fn set_scroll(&mut self, metrics: Option<ScrollMetrics>) {
        self.scrollbar_rect = self
            .scrollbar_gutter
            .filter(|_| metrics.is_some_and(Self::scrollbar_visible));
    }

    /// Whether `pane`, as committed to a client, has the geometry this pane
    /// is laid out with now: the same outer and content rects, and a committed
    /// scrollbar track, if any, in the reserved gutter.
    pub(crate) fn matches_committed(&self, pane: &PaneSurfacePane) -> bool {
        surface_rect(self.rect) == pane.rect
            && surface_rect(self.inner_rect) == pane.inner_rect
            && (pane.scrollbar_rect.is_none()
                || pane.scrollbar_rect == self.scrollbar_gutter.map(surface_rect))
    }

    /// What the scrollbar column holds for a client whose committed track was
    /// `previous`: the track when it draws and blanks when it was just taken
    /// away. `None` when the pane has no track now and had none.
    pub(crate) fn scrollbar_paint(
        &self,
        previous: Option<SurfaceRect>,
        metrics: Option<ScrollMetrics>,
    ) -> Option<ScrollbarPaint> {
        let now = self.scrollbar_rect.map(surface_rect);
        Some(ScrollbarPaint {
            rect: now.or(previous)?,
            metrics: now.and(metrics),
            focused: self.is_focused,
        })
    }

    /// The cursor the pane shows: geometry belongs to the viewing client,
    /// while agent identity belongs to the pane.
    pub(crate) fn cursor(&self, state: &AppState, runtime: &PaneRuntime) -> Option<CursorState> {
        let area = self.inner_rect;
        // One read decides both the cursor and whether a synchronized update
        // holds it back.
        let cursor = match runtime.read().cursor(area) {
            shepr_mux::pane::CursorRead::Deferred => return None,
            shepr_mux::pane::CursorRead::Shown(cursor) => Some(cursor),
            shepr_mux::pane::CursorRead::Unavailable => None,
        };
        let scrolled_back = super::panes::pane_is_scrolled_back(runtime);
        let reveal = state.settings().reveal_hidden_cursor_for_cjk_ime
            && state.settings().cjk_ime_agents.includes(
                state
                    .terminal(self.id)
                    .and_then(|terminal| terminal.ownership().detected_agent()),
            );

        if let Some(cursor) = cursor {
            let visible = if reveal {
                !scrolled_back
            } else {
                cursor.visible && !scrolled_back
            };
            Some(CursorState {
                x: cursor.x,
                y: cursor.y,
                visible,
                shape: if reveal && visible {
                    state.settings().cjk_ime_cursor_shape
                } else {
                    cursor.shape
                },
            })
        } else if reveal && !scrolled_back {
            Some(CursorState {
                x: area.x,
                y: area.y,
                visible: true,
                shape: state.settings().cjk_ime_cursor_shape,
            })
        } else {
            None
        }
    }
}
