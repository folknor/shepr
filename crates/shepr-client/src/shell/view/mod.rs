//! The view model of the frame on screen. A composition has three phases:
//! `resolve::resolve_frame` lays out every element and resolves every scroll position,
//! `draw::draw_frame` draws only what the view says, and `commit_frame` is the one place a
//! composition writes shell state. The first two read the shell by shared reference; the
//! commit runs only after the host terminal took the frame.

mod draw;
pub(in crate::shell) mod list;
pub(in crate::shell) mod resolve;

use ratatui::layout::Rect;
use shepr_protocol::FrameData;
use shepr_term::scroll::ListScroll;

use crate::endpoint::ClientEndpointStatus;
use crate::shell::config::ClientShellLayout;
use crate::shell::navigation::location::{Location, PinnedLocation};
use crate::shell::overlays::{OverlayScroll, OverlayView};
use crate::shell::presentation::LastComposition;
use crate::shell::sidebar::layout::{AgentSlot, CollapsedSlot, ExpandedSlot, SidebarView};
use crate::shell::sidebar::scroll::SidebarScrollResolution;
use crate::shell::state::ClientShellState;

/// What the last composed frame shows and where. Produced by `resolve_frame`, drawn by
/// `draw_frame`, kept by `Presentation` until the next composition. Input aims at it because it
/// describes the screen.
pub(in crate::shell) struct ShellView {
    pub(in crate::shell) size: (u16, u16),
    /// The layout this frame was drawn with (`ClientShellState::layout` at `size`).
    pub(in crate::shell) layout: ClientShellLayout,
    /// The pane cells were drawn (a snapshot and a paired surface existed).
    has_surface: bool,
    placeholder: Option<Placeholder>,
    sidebar: SidebarView,
    /// The workspace the sidebar highlights as selected: the Navigate preview, or the pending
    /// focus highlight outside Navigate.
    selected: Option<PinnedLocation>,
    /// The panes the surface drew, clipped to the pane area. Input aims at them only while
    /// `hits_live`; highlights and the copy cursor follow them either way.
    panes: Vec<PaneHit>,
    /// Empty when the mouse is not captured, the surface overflows its area, or the endpoint
    /// is unusable.
    splits: Vec<PaneSplitHit>,
    /// The presented endpoint is usable, so pane input has a target.
    hits_live: bool,
    /// The screen cell of the copy cursor, when its pane is drawn, coherent with the copy
    /// state, and the cursor row is inside the pane's viewport.
    copy_cursor: Option<(u16, u16)>,
    lifecycle: Option<LifecycleBanner>,
    notice: Option<NoticeCard>,
    mode_bar_area: Rect,
    /// The open overlay as laid out; `None` when no overlay is open or it does not fit.
    pub(in crate::shell) overlay: Option<OverlayView>,
    /// `ui.mouse_capture`: when false, the sidebar chrome hits are inert.
    chrome_armed: bool,
}

struct Placeholder {
    area: Rect,
    message: String,
}

struct LifecycleBanner {
    rect: Rect,
    label: String,
    status: ClientEndpointStatus,
}

struct NoticeCard {
    rect: Rect,
}

/// A frame resolved but not yet drawn or stored.
struct ResolvedFrame {
    view: ShellView,
    sidebar: SidebarScrollResolution,
    /// A size change while navigating asked for the selected workspace to be revealed, and the
    /// sidebar had no body to reveal it in: the request stays pending.
    carry_selected_reveal: bool,
    overlay_scroll: Option<OverlayScroll>,
}

/// A frame drawn from a view: the cells and what the drawing did to pane output.
struct DrawnFrame {
    frame: FrameData,
    effects: LastComposition,
}

static EMPTY_VIEW: ShellView = ShellView::empty_at((0, 0));

impl ShellView {
    /// A view that shows nothing and offers no targets.
    pub(in crate::shell) const fn empty_at(size: (u16, u16)) -> Self {
        Self {
            size,
            layout: ClientShellLayout {
                sidebar: Rect::ZERO,
                pane_surface: Rect::ZERO,
            },
            has_surface: false,
            placeholder: None,
            sidebar: SidebarView::Hidden,
            selected: None,
            panes: Vec::new(),
            splits: Vec::new(),
            hits_live: false,
            copy_cursor: None,
            lifecycle: None,
            notice: None,
            mode_bar_area: Rect::ZERO,
            overlay: None,
            chrome_armed: false,
        }
    }

    pub(in crate::shell) fn empty_ref() -> &'static ShellView {
        &EMPTY_VIEW
    }

    /// The panes input can aim at: none while the endpoint is unusable.
    pub(in crate::shell) fn pane_hits(&self) -> &[PaneHit] {
        if self.hits_live { &self.panes } else { &[] }
    }

    pub(in crate::shell) fn pane_splits(&self) -> &[PaneSplitHit] {
        &self.splits
    }

    pub(in crate::shell) fn pane_hit_mut(
        &mut self,
        pane_id: &shepr_protocol::PublicPaneId,
    ) -> Option<&mut PaneHit> {
        self.panes.iter_mut().find(|hit| &hit.pane_id == pane_id)
    }

    pub(in crate::shell) fn machines(&self) -> impl Iterator<Item = &MachineHit> + '_ {
        let (collapsed, expanded): (&[CollapsedSlot], &[ExpandedSlot]) =
            match (&self.sidebar, self.chrome_armed) {
                (SidebarView::Collapsed(view), true) => {
                    (view.workspaces.slots.as_slice(), Default::default())
                }
                (SidebarView::Expanded(view), true) => {
                    (Default::default(), view.workspaces.slots.as_slice())
                }
                _ => (Default::default(), Default::default()),
            };
        collapsed
            .iter()
            .filter_map(|slot| match slot {
                CollapsedSlot::Machine { hit, .. } => Some(hit),
                CollapsedSlot::Workspace { .. } => None,
            })
            .chain(expanded.iter().filter_map(|slot| match slot {
                ExpandedSlot::Machine { hit, .. } => Some(hit),
                ExpandedSlot::Workspace { .. } => None,
            }))
    }

    pub(in crate::shell) fn workspaces(
        &self,
    ) -> impl DoubleEndedIterator<Item = &WorkspaceHit> + '_ {
        let (collapsed, expanded): (&[CollapsedSlot], &[ExpandedSlot]) =
            match (&self.sidebar, self.chrome_armed) {
                (SidebarView::Collapsed(view), true) => {
                    (view.workspaces.slots.as_slice(), Default::default())
                }
                (SidebarView::Expanded(view), true) => {
                    (Default::default(), view.workspaces.slots.as_slice())
                }
                _ => (Default::default(), Default::default()),
            };
        collapsed
            .iter()
            .filter_map(|slot| match slot {
                CollapsedSlot::Workspace { hit, .. } => Some(hit),
                CollapsedSlot::Machine { .. } => None,
            })
            .chain(expanded.iter().filter_map(|slot| match slot {
                ExpandedSlot::Workspace { hit, .. } => Some(hit),
                ExpandedSlot::Machine { .. } => None,
            }))
    }

    pub(in crate::shell) fn agents(&self) -> impl Iterator<Item = &AgentHit> + '_ {
        let slots: &[AgentSlot] = match (&self.sidebar, self.chrome_armed) {
            (SidebarView::Collapsed(view), true) => view.agents.as_slice(),
            (SidebarView::Expanded(view), true) => view
                .agents
                .as_ref()
                .and_then(|panel| panel.list.as_ref())
                .map_or(Default::default(), |list| list.slots.as_slice()),
            _ => Default::default(),
        };
        slots.iter().map(|slot| &slot.hit)
    }

    /// The workspace list's body, collapsed or expanded.
    pub(in crate::shell) fn workspace_body(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Hidden => Rect::default(),
            SidebarView::Collapsed(view) => view.workspaces.body,
            SidebarView::Expanded(view) => view.workspaces.body,
        }
    }

    /// The highest start the workspace list can scroll to.
    pub(in crate::shell) fn workspace_max_scroll(&self) -> usize {
        match &self.sidebar {
            SidebarView::Hidden => 0,
            SidebarView::Collapsed(view) => view.workspaces.scroll.max_start(),
            SidebarView::Expanded(view) => view.workspaces.scroll.max_start(),
        }
    }

    /// The expanded workspace list's scroll, which its scrollbar drags.
    pub(in crate::shell) fn workspace_scroll_metrics(&self) -> Option<ListScroll> {
        match &self.sidebar {
            SidebarView::Expanded(view) => Some(view.workspaces.scroll),
            SidebarView::Hidden | SidebarView::Collapsed(_) => None,
        }
    }

    pub(in crate::shell) fn workspace_scrollbar(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Expanded(view) if self.chrome_armed => {
                view.workspaces.scrollbar.unwrap_or_default()
            }
            _ => Rect::default(),
        }
    }

    fn agent_list(&self) -> Option<&crate::shell::view::list::ListView<AgentSlot>> {
        match &self.sidebar {
            SidebarView::Expanded(view) => {
                view.agents.as_ref().and_then(|panel| panel.list.as_ref())
            }
            SidebarView::Hidden | SidebarView::Collapsed(_) => None,
        }
    }

    pub(in crate::shell) fn agent_body(&self) -> Rect {
        self.agent_list()
            .map_or_else(Rect::default, |list| list.body)
    }

    pub(in crate::shell) fn agent_max_scroll(&self) -> usize {
        self.agent_list().map_or(0, |list| list.scroll.max_start())
    }

    /// The agent list's scroll, which its scrollbar drags. `None` without rows.
    pub(in crate::shell) fn agent_scroll_metrics(&self) -> Option<ListScroll> {
        self.agent_list()
            .filter(|list| !list.slots.is_empty())
            .map(|list| list.scroll)
    }

    pub(in crate::shell) fn agent_scrollbar(&self) -> Rect {
        if !self.chrome_armed {
            return Rect::default();
        }
        self.agent_list()
            .and_then(|list| list.scrollbar)
            .unwrap_or_default()
    }

    pub(in crate::shell) fn agent_sort_toggle(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Expanded(view) if self.chrome_armed => view
                .agents
                .as_ref()
                .and_then(|panel| panel.sort_toggle)
                .unwrap_or_default(),
            _ => Rect::default(),
        }
    }

    pub(in crate::shell) fn sidebar_divider(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Expanded(view) if self.chrome_armed => view.divider,
            _ => Rect::default(),
        }
    }

    pub(in crate::shell) fn section_divider(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Expanded(view) if self.chrome_armed => view.section_divider,
            _ => Rect::default(),
        }
    }

    pub(in crate::shell) fn sidebar_toggle(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Hidden => Rect::default(),
            SidebarView::Collapsed(view) => view.toggle,
            SidebarView::Expanded(view) => view.toggle,
        }
    }

    /// Only drawn, and so only offered, with the mouse captured.
    pub(in crate::shell) fn new_workspace(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Expanded(view) => view
                .footer
                .as_ref()
                .map_or_else(Rect::default, |footer| footer.new_workspace),
            SidebarView::Hidden | SidebarView::Collapsed(_) => Rect::default(),
        }
    }

    /// Only drawn, and so only offered, with the mouse captured.
    pub(in crate::shell) fn global_launcher(&self) -> Rect {
        match &self.sidebar {
            SidebarView::Expanded(view) => view
                .footer
                .as_ref()
                .map_or_else(Rect::default, |footer| footer.global_launcher),
            SidebarView::Hidden | SidebarView::Collapsed(_) => Rect::default(),
        }
    }

    pub(in crate::shell) fn notification_toast(&self) -> Rect {
        self.notice
            .as_ref()
            .map_or_else(Rect::default, |notice| notice.rect)
    }
}

#[derive(Clone)]
pub(in crate::shell) struct MachineHit {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) status_badge: Rect,
    pub(in crate::shell) collapse_toggle: Rect,
    pub(in crate::shell) location: Location,
}

#[derive(Clone)]
pub(in crate::shell) struct PaneHit {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) inner_rect: Rect,
    pub(in crate::shell) scrollbar_rect: Option<Rect>,
    pub(in crate::shell) scroll: Option<shepr_term::ScrollMetrics>,
    pub(in crate::shell) pane_id: shepr_protocol::PublicPaneId,
    pub(in crate::shell) mouse_reporting: bool,
    pub(in crate::shell) pixel_mouse: shepr_term::mouse::PanePixelMouse,
    /// The pane's content size in cells as the server laid it out, width then
    /// height, whether or not the clip cut it. `inner_rect` is only the visible part.
    pub(in crate::shell) pane_size: (u16, u16),
    /// The pane's full content grid when it is shown unclipped, `None` when
    /// the clip cut it (pixels cannot be mapped then).
    pub(in crate::shell) presented: Option<shepr_core::geometry::GridSize>,
}

/// A surface-local wire rect placed on screen with the pane surface's top-left
/// cell at `origin`. Every hit rect (panes and splits) is translated here.
/// Surface-local rects stay the protocol's `SurfaceRect` and screen rects are
/// ratatui `Rect`s, so the two cannot be mixed up without a conversion; a
/// separate screen rect type would add churn without closing a gap.
fn surface_rect_on_screen(origin: (u16, u16), rect: shepr_protocol::SurfaceRect) -> Rect {
    Rect::new(
        origin.0.saturating_add(rect.x),
        origin.1.saturating_add(rect.y),
        rect.width,
        rect.height,
    )
}

impl PaneHit {
    /// The drawn part of the pane's content, columns then rows, counted from its top-left
    /// cell. It is `pane_size` unless the clip cut the pane.
    pub(in crate::shell) fn visible_size(&self) -> (u16, u16) {
        (
            self.inner_rect.width.min(self.pane_size.0),
            self.inner_rect.height.min(self.pane_size.1),
        )
    }

    /// Builds a hit from a wire pane, whose rects are surface-local, by
    /// translating them to screen cells and clipping them to `clip`.
    pub(in crate::shell) fn from_wire(
        pane: &shepr_protocol::PaneSurfacePane,
        origin: (u16, u16),
        clip: Rect,
    ) -> Option<Self> {
        let offset = |rect| surface_rect_on_screen(origin, rect);
        let inner_rect = offset(pane.inner_rect);
        let visible_inner = inner_rect.intersection(clip);
        if visible_inner.is_empty() {
            return None;
        }
        let clipped = inner_rect != visible_inner;
        Some(Self {
            rect: offset(pane.rect).intersection(clip),
            inner_rect: visible_inner,
            scrollbar_rect: pane
                .scrollbar_rect
                .map(offset)
                .map(|rect| rect.intersection(clip))
                .filter(|rect| !rect.is_empty()),
            scroll: pane.scroll,
            pane_id: pane.pane_id,
            mouse_reporting: pane.mouse_reporting,
            pixel_mouse: pane.pixel_mouse,
            pane_size: (pane.inner_rect.width, pane.inner_rect.height),
            presented: shepr_core::geometry::GridSize::new(
                pane.inner_rect.width,
                pane.inner_rect.height,
            )
            .filter(|_| !clipped),
        })
    }
}

#[derive(Clone)]
pub(in crate::shell) struct PaneSplitHit {
    pub(in crate::shell) direction: shepr_protocol::PaneSurfaceSplitDirection,
    pub(in crate::shell) pos: u16,
    pub(in crate::shell) area: Rect,
    pub(in crate::shell) hit_rect: Rect,
    pub(in crate::shell) path: Vec<shepr_core::layout::SplitBranch>,
    /// The layout epoch the server published `path` at.
    pub(in crate::shell) epoch: shepr_core::layout::LayoutEpoch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::shell) struct AgentHit {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) location: Location,
}

#[derive(Clone)]
pub(in crate::shell) struct WorkspaceHit {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) location: Location,
}

/// A frame composed but not yet on screen: the cells to write to the host, and what
/// composing them decided, which the shell keeps only once the host took the cells.
pub(crate) struct ComposedFrame {
    pub(crate) frame: FrameData,
    pub(crate) commit: FrameCommit,
}

/// What a composition decided (the view, scroll positions and drawing effects), held
/// until the frame it belongs to reaches the host.
pub(crate) struct FrameCommit {
    resolved: ResolvedFrame,
    effects: LastComposition,
}

impl ClientShellState {
    /// Composes one frame: resolves the view and draws it, both by shared reference.
    /// Nothing is stored: the caller writes the frame to the host and then hands the
    /// commit to `commit_frame`, so a frame the host refused leaves no trace (a notice's
    /// lifetime does not start, a reveal stays pending, input keeps aiming at the frame
    /// on screen). `None` when the presented surface is held unpaired (the last frame
    /// stays on screen) or the canvas refuses (likewise).
    pub(crate) fn compose_frame(&self, cols: u16, rows: u16) -> Option<ComposedFrame> {
        if !self
            .presentation
            .can_draw(self.endpoints.active.snapshot().is_some())
        {
            return None;
        }
        let resolved = resolve::resolve_frame(self, cols, rows);
        let drawn = draw::draw_frame(self, &resolved.view)?;
        Some(ComposedFrame {
            frame: drawn.frame,
            commit: FrameCommit {
                resolved,
                effects: drawn.effects,
            },
        })
    }

    /// Keeps what a composition decided, once its frame is on screen. The one place a
    /// composition writes shell state.
    pub(crate) fn commit_frame(&mut self, commit: FrameCommit) {
        let FrameCommit { resolved, effects } = commit;
        self.sidebar_scroll.commit(&resolved.sidebar);
        if resolved.carry_selected_reveal {
            self.sidebar_scroll.reveal_selected_workspace();
        }
        // The scroll an overlay resolved is stored with the frame, so input moves from what
        // was drawn. An overlay that did not fit resolved nothing and keeps its scroll.
        if let (Some(scroll), Some(overlay)) = (resolved.overlay_scroll, self.overlay.as_mut()) {
            overlay.commit_scroll(scroll);
        }
        // Both pane layers pass through the notice stage, so its lifetime starts here.
        self.notices.drawn(self.now);
        self.mouse_selection.frame_drawn();
        self.presentation.commit(resolved.view, effects, self.now);
    }

    /// The view of the frame on screen, `None` before the first frame is drawn.
    pub(in crate::shell) fn view(&self) -> Option<&ShellView> {
        self.presentation.view()
    }

    /// The panes of the frame on screen that input can aim at.
    pub(in crate::shell) fn pane_hits(&self) -> &[PaneHit] {
        self.presentation.pane_hits()
    }
}

impl ClientShellState {
    /// Composes a frame and commits it at once, as if the host took it.
    #[cfg(test)]
    pub(crate) fn compose(&mut self, cols: u16, rows: u16) -> Option<FrameData> {
        let ComposedFrame { frame, commit } = self.compose_frame(cols, rows)?;
        self.commit_frame(commit);
        Some(frame)
    }
}

// The overlay readers below are for tests: overlay input reads the `OverlayView` itself.
#[cfg(test)]
impl ShellView {
    pub(in crate::shell) fn help_popup(&self) -> Rect {
        match &self.overlay {
            Some(OverlayView::Help(help)) => help.popup,
            _ => Rect::default(),
        }
    }

    pub(in crate::shell) fn navigator_popup(&self) -> Rect {
        match &self.overlay {
            Some(OverlayView::Navigator(navigator)) => navigator.popup,
            _ => Rect::default(),
        }
    }

    pub(in crate::shell) fn navigator_search(&self) -> Rect {
        match &self.overlay {
            Some(OverlayView::Navigator(navigator)) => navigator.search,
            _ => Rect::default(),
        }
    }

    pub(in crate::shell) fn navigator_scrollbar(&self) -> Rect {
        match &self.overlay {
            Some(OverlayView::Navigator(navigator)) => navigator.list.scrollbar.unwrap_or_default(),
            _ => Rect::default(),
        }
    }

    pub(in crate::shell) fn navigator_scroll_metrics(&self) -> Option<ListScroll> {
        match &self.overlay {
            Some(OverlayView::Navigator(navigator)) => Some(navigator.list.scroll),
            _ => None,
        }
    }

    /// The navigator's drawn rows with their targets, top to bottom.
    pub(in crate::shell) fn navigator_rows(&self) -> impl Iterator<Item = (Rect, &Location)> + '_ {
        let navigator = match &self.overlay {
            Some(OverlayView::Navigator(navigator)) => Some(navigator),
            _ => None,
        };
        navigator.into_iter().flat_map(|navigator| {
            navigator.list.slots.iter().filter_map(move |slot| {
                navigator
                    .rows
                    .get(slot.row)
                    .map(|row| (slot.rect, &row.target))
            })
        })
    }

    pub(in crate::shell) fn global_menu_rows(&self) -> &[(Rect, usize)] {
        match &self.overlay {
            Some(OverlayView::GlobalMenu(menu)) => &menu.rows,
            _ => &[],
        }
    }

    pub(in crate::shell) fn context_menu_rows(&self) -> &[(Rect, usize)] {
        match &self.overlay {
            Some(OverlayView::ContextMenu(menu)) => &menu.rows,
            _ => &[],
        }
    }
}

#[cfg(test)]
impl ShellView {
    /// The screen cell the copy cursor was drawn at, if it was drawn.
    pub(in crate::shell) fn copy_cursor(&self) -> Option<(u16, u16)> {
        self.copy_cursor
    }
}

#[cfg(test)]
impl ClientShellState {
    pub(in crate::shell) fn drawn(&self) -> &ShellView {
        self.view().expect("no frame was drawn")
    }

    pub(in crate::shell) fn pane_hits_mut(&mut self) -> &mut Vec<PaneHit> {
        &mut self
            .presentation
            .view_mut()
            .expect("no frame was drawn")
            .panes
    }
}

#[cfg(test)]
impl ShellView {
    /// A view whose expanded sidebar lists `hit` as its one machine row.
    pub(in crate::shell) fn with_machine_hit(hit: MachineHit) -> Self {
        let area = Rect::new(0, 0, 24, 10);
        let mut view = Self::empty_at((80, 24));
        view.sidebar = SidebarView::Expanded(crate::shell::sidebar::layout::ExpandedSidebarView {
            area,
            divider: Rect::default(),
            section_divider: Rect::default(),
            toggle: Rect::default(),
            workspace_area: area,
            workspaces: list::ListView {
                body: area,
                scroll: ListScroll::new(0, 0, 0),
                scrollbar: None,
                slots: vec![ExpandedSlot::Machine { hit, endpoint: 0 }],
            },
            drop_indicator: None,
            footer: None,
            agents: None,
        });
        view.chrome_armed = true;
        view
    }
}

#[cfg(test)]
mod tests {
    use crate::endpoint::ClientEndpointId;
    use crate::endpoint::EndpointFailureStatus;
    use crate::shell::config::ClientShellConfig;
    use crate::shell::notices::ClientEndpointNoticeKind;
    use crate::shell::state::ClientShellState;
    use shepr_config::ClientConfig;
    use shepr_config::SidebarCollapsedModeConfig;
    use shepr_protocol::FrameData;

    fn frame_row_text(frame: &FrameData, y: u16) -> String {
        let start = usize::from(y) * usize::from(frame.width());
        frame.cells()[start..start + usize::from(frame.width())]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn placeholder_lifecycle_and_notice_leave_the_hidden_sidebar_header_clear() {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.config.sidebar_collapsed_mode = SidebarCollapsedModeConfig::Hidden;
        state.chrome.set_collapsed(true);
        state.set_endpoint_status(
            &ClientEndpointId::Local,
            EndpointFailureStatus::Reconnecting,
        );
        assert!(state.push_endpoint_notice(
            ClientEndpointNoticeKind::Unavailable,
            crate::shell::notices::NoticeCode::EndpointUnavailable,
            "Server unavailable",
            "Waiting for the server",
        ));

        let frame = state.compose(34, 12).expect("placeholder frame");

        let lifecycle_row = frame_row_text(&frame, 0);
        assert!(lifecycle_row.contains("reconnecting"));
        assert!(!lifecycle_row.contains("Local:"));
        assert!(frame_row_text(&frame, 1).contains("spaces"));
        assert!(state.drawn().notification_toast().y >= 2);
    }

    #[test]
    fn online_placeholder_notice_starts_below_the_expanded_sidebar_header() {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.chrome.set_collapsed(false);
        state.chrome.set_width(24);
        state.set_snapshot(Box::new(crate::shell::tests::snapshot()));
        assert!(state.push_endpoint_notice(
            ClientEndpointNoticeKind::Unavailable,
            crate::shell::notices::NoticeCode::EndpointUnavailable,
            "Server unavailable",
            "Waiting for the server",
        ));

        let frame = state.compose(80, 12).expect("placeholder frame");

        assert!(frame_row_text(&frame, 0).contains("spaces"));
        assert!(state.drawn().notification_toast().y >= 1);
    }
}
