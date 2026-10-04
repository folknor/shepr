use ratatui::layout::Rect;

use std::collections::HashMap;

use super::PaneSurface;
use super::panes::{render_panes, visible_chromes, workspace_runtime};
use crate::app::AppState;
use shepr_core::chrome::PaneChrome;
use shepr_core::layout::SplitBorder;
use shepr_mux::pane::PaneRuntimeRegistry;
use shepr_mux::workspace::Workspace;
use shepr_protocol::{CursorState, FrameData, WorkspaceId};

/// A workspace resolved against the state of one render pass: its position and
/// id. Resolved once per client per pass, then carried to every reader, which
/// looks the workspace up by position and checks the id, so a target that went
/// stale draws nothing instead of drawing another workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SurfaceTarget {
    pub(crate) index: usize,
    pub(crate) id: WorkspaceId,
}

impl SurfaceTarget {
    /// The workspace this target names in `app`: the one at `index`, if it
    /// still has `id`. O(1).
    pub(crate) fn workspace(self, app: &AppState) -> Option<&Workspace> {
        app.workspaces()
            .as_slice()
            .get(self.index)
            .filter(|workspace| workspace.id() == self.id)
    }
}

pub(crate) struct SurfaceLayout {
    pub(crate) target: Option<SurfaceTarget>,
    pub(crate) panes: Vec<PaneSurface>,
    pub(crate) split_borders: Vec<SplitBorder>,
}

#[derive(Clone, Copy)]
pub(crate) struct SurfaceView<'a> {
    pub(crate) target: Option<SurfaceTarget>,
    pub(crate) panes: &'a [PaneSurface],
    pub(crate) split_borders: &'a [SplitBorder],
}

/// One workspace laid out for one client surface of size `area`. Pure: it
/// reads the state and the runtimes' screen modes and resizes nothing. Each
/// client renders the workspace its own location names at its own size; which
/// size the workspace's PTYs get is decided separately, by the server's
/// geometry rule.
pub(crate) fn compute_surface_for(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    target: Option<SurfaceTarget>,
    area: Rect,
) -> SurfaceLayout {
    let workspace = target.and_then(|target| target.workspace(app));
    let split_borders = workspace.map_or_default(|workspace| {
        if workspace.tree().zoomed() {
            Vec::new()
        } else {
            workspace.tree().layout().splits(super::core_rect(area))
        }
    });
    let panes = workspace.map_or_else(Vec::new, |workspace| {
        compute_pane_surfaces(app, terminal_runtimes, workspace, area)
    });

    SurfaceLayout {
        target,
        panes,
        split_borders,
    }
}

/// How each visible pane of `workspace` looks in `area`, settled against its
/// runtime's screen mode and scrollback. Reads the runtimes and changes
/// nothing; the server's geometry path sizes PTYs from the same description.
pub(crate) fn compute_pane_surfaces(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    workspace: &Workspace,
    area: Rect,
) -> Vec<PaneSurface> {
    visible_chromes(app, workspace, super::core_rect(area))
        .into_iter()
        .map(|chrome| {
            let runtime = workspace_runtime(workspace, terminal_runtimes, chrome.id);
            let scrollbars = app.settings().pane_scrollbars;
            PaneSurface::settle(
                chrome,
                scrollbars,
                scrollbars && runtime.is_some_and(|rt| rt.read().alternate_screen_active()),
                || runtime.and_then(|rt| rt.read().scroll_metrics()),
            )
        })
        .collect()
}

/// The chrome of workspaces' visible panes at a surface size, computed once
/// per workspace and size. Several clients can view one workspace at the same
/// size, and the chrome depends on nothing else, so a pass that checks each
/// client's committed panes against it shares the result.
#[derive(Default)]
pub(crate) struct PaneLayoutCache {
    layouts: HashMap<(WorkspaceId, u16, u16), Option<Vec<PaneChrome>>>,
}

impl PaneLayoutCache {
    /// The visible panes' chrome of `workspace_id` in a `width` by `height`
    /// surface; `None` when the workspace is gone.
    pub(crate) fn chromes(
        &mut self,
        app: &AppState,
        workspace_id: WorkspaceId,
        width: u16,
        height: u16,
    ) -> Option<&[PaneChrome]> {
        self.layouts
            .entry((workspace_id, width, height))
            .or_insert_with(|| {
                let workspace = app.workspace(&workspace_id)?;
                Some(visible_chromes(
                    app,
                    workspace,
                    shepr_core::geometry::Rect::new(0, 0, width, height),
                ))
            })
            .as_deref()
    }
}

/// Draws the surface's panes and chrome into `frame`, whose size is the
/// client's surface. Pane cells, typed underline shapes and hyperlinks
/// included, are written directly in wire form. Returns how each pane's draw
/// went, so the caller learns of a pane that was held back (a synchronized
/// update, an unreadable core) from the draw itself.
pub(crate) fn render_surface(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    surface: SurfaceView<'_>,
    frame: &mut FrameData,
) -> Vec<(shepr_core::layout::PaneId, shepr_mux::pane::PaneDraw)> {
    render_panes(
        app,
        terminal_runtimes,
        frame,
        surface.target,
        surface.panes,
        surface.split_borders,
    )
}

pub(crate) fn surface_cursor(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    surface: SurfaceView<'_>,
) -> Option<CursorState> {
    let workspace = surface.target?.workspace(app)?;
    let pane = surface.panes.iter().find(|pane| pane.is_focused)?;
    let runtime = workspace_runtime(workspace, terminal_runtimes, pane.id)?;
    pane.cursor(app, runtime)
}

#[cfg(test)]
impl PaneLayoutCache {
    pub(crate) fn len(&self) -> usize {
        self.layouts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_core::layout::Direction;
    use shepr_mux::workspace::Workspace;

    #[tokio::test]
    async fn explicit_surface_layout_drives_render_cursor_and_hyperlinks() {
        let uri = "https://example.com/surface";
        let mut workspace = Workspace::test_new("shell-workspace");
        let left = workspace.tree().root();
        let right = workspace.test_split(Direction::Horizontal);
        let mut runtimes = PaneRuntimeRegistry::new();
        runtimes.insert(
            left,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
                20,
                8,
                format!("\x1b]8;;{uri}\x1b\\LEFT\x1b]8;;\x1b\\").as_bytes(),
            ),
        );
        runtimes.insert(
            right,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 8, b"RIGHT"),
        );

        let mut app = AppState::test_new();
        app.test_set_workspaces(vec![workspace]);
        app.seed_bookmark_index(Some(0));

        let full_area = Rect::new(0, 0, 106, 20);
        let area = full_area;
        let target = SurfaceTarget {
            index: 0,
            id: app.ws(0).id(),
        };
        let surface = compute_surface_for(&app, &runtimes, Some(target), area);
        assert_eq!(surface.panes.len(), 2);
        assert!(!surface.split_borders.is_empty());

        // The recorded layout area of the workspace is session geometry for spawn
        // sizing; a client surface is drawn from its own layout alone.
        app.test_record_all_workspace_areas(Rect::new(9, 8, 7, 6));

        let surface_view = SurfaceView {
            target: surface.target,
            panes: &surface.panes,
            split_borders: &surface.split_borders,
        };
        let mut frame =
            FrameData::blank(full_area.width, full_area.height).expect("test frame size is valid");
        render_surface(&app, &runtimes, surface_view, &mut frame);

        let rendered = frame
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(rendered.contains("LEFT"), "surface: {rendered:?}");
        assert!(rendered.contains("RIGHT"), "surface: {rendered:?}");
        assert!(!rendered.contains("shell-workspace"));

        // The link is on the cells of the linked text, in the frame's table.
        assert_eq!(frame.hyperlinks(), [uri.to_owned()]);
        let linked = frame
            .cells()
            .iter()
            .filter(|cell| cell.hyperlink.is_some())
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert_eq!(linked, "LEFT");
        assert!(surface_cursor(&app, &runtimes, surface_view,).is_some());
    }

    #[test]
    fn a_stale_surface_target_draws_no_workspace() {
        let mut app = AppState::test_new();
        app.test_set_workspaces(vec![
            Workspace::test_new("first"),
            Workspace::test_new("second"),
        ]);
        let runtimes = PaneRuntimeRegistry::new();
        let area = Rect::new(0, 0, 80, 24);
        let first = app.ws(0).id();
        let second = app.ws(1).id();

        let live = SurfaceTarget {
            index: 1,
            id: second,
        };
        assert_eq!(
            compute_surface_for(&app, &runtimes, Some(live), area)
                .panes
                .len(),
            1
        );

        // The position holds another id: the workspace moved since the target
        // was resolved.
        let stale = SurfaceTarget {
            index: 0,
            id: second,
        };
        assert!(stale.workspace(&app).is_none());
        let layout = compute_surface_for(&app, &runtimes, Some(stale), area);
        assert!(layout.panes.is_empty());
        assert!(layout.split_borders.is_empty());

        let past_the_end = SurfaceTarget {
            index: 2,
            id: first,
        };
        assert!(
            compute_surface_for(&app, &runtimes, Some(past_the_end), area)
                .panes
                .is_empty()
        );
    }
}
