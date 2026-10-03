use ratatui::layout::Rect;

use super::PaneResizer;
use super::panes::{compute_pane_infos_for_workspace, render_panes, resize_pane_infos};
use crate::app::AppState;
use shepr_core::layout::SplitBorder;
use shepr_mux::pane::PaneRuntimeRegistry;
use shepr_mux::workspace::PaneChromeInfo as PaneInfo;
use shepr_protocol::{CursorState, FrameData, WorkspaceId};

pub(crate) struct SurfaceLayout {
    pub(crate) target: Option<WorkspaceId>,
    pub(crate) pane_infos: Vec<PaneInfo>,
    pub(crate) split_borders: Vec<SplitBorder>,
}

#[derive(Clone, Copy)]
pub(crate) struct SurfaceView<'a> {
    pub(crate) target: Option<&'a WorkspaceId>,
    pub(crate) pane_infos: &'a [PaneInfo],
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
    target: Option<WorkspaceId>,
    area: Rect,
) -> SurfaceLayout {
    let resolved = target.as_ref().and_then(|id| app.workspace_index(id));
    let workspace = resolved.and_then(|workspace_index| app.workspaces.get(workspace_index));
    let split_borders = workspace.map_or_default(|workspace| {
        if workspace.zoomed() {
            Vec::new()
        } else {
            workspace
                .layout()
                .splits(shepr_mux::workspace::layout_rect(area))
        }
    });
    let pane_infos = resolved.map_or_else(Vec::new, |workspace_index| {
        compute_pane_infos_for_workspace(app, terminal_runtimes, workspace_index, area)
    });

    SurfaceLayout {
        target,
        pane_infos,
        split_borders,
    }
}

/// Resizes the visible panes of one workspace to their content rects in
/// `area`: the explicit geometry path the server's PTY size rule runs through.
pub(crate) fn resize_surface(
    app: &AppState,
    resizer: &mut PaneResizer<'_>,
    workspace_index: usize,
    area: Rect,
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) {
    let pane_infos = compute_pane_infos_for_workspace(app, resizer.runtimes, workspace_index, area);
    resize_pane_infos(app, resizer, workspace_index, &pane_infos, cell_size);
}

/// Draws the surface's panes and chrome into `frame`, whose size is the
/// client's surface. Pane cells, typed underline shapes and hyperlinks
/// included, are written directly in wire form.
pub(crate) fn render_surface(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    surface: SurfaceView<'_>,
    frame: &mut FrameData,
) {
    render_panes(
        app,
        terminal_runtimes,
        frame,
        surface.target,
        surface.pane_infos,
        surface.split_borders,
    );
}

pub(crate) fn surface_cursor(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    surface: SurfaceView<'_>,
) -> Option<CursorState> {
    let ws_idx = app.workspace_index(surface.target?)?;
    let info = surface.pane_infos.iter().find(|info| info.is_focused)?;
    let runtime = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)?;
    pane_cursor(app, runtime, ws_idx, info.id, info.inner_rect)
}

/// Cursor policy shared by complete surfaces and retained updates. Geometry
/// belongs to the viewing client, while agent identity belongs to the pane.
pub(crate) fn pane_cursor(
    app: &AppState,
    runtime: &shepr_mux::pane::PaneRuntime,
    ws_idx: usize,
    pane_id: shepr_core::layout::PaneId,
    area: Rect,
) -> Option<CursorState> {
    if runtime.read().synchronized_output_active() {
        return None;
    }
    let scrolled_back = super::panes::pane_is_scrolled_back(runtime);
    let reveal = app.settings.reveal_hidden_cursor_for_cjk_ime
        && (app.settings.cjk_ime_agents.is_empty() || {
            let detected = app
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.terminal_id(pane_id))
                .and_then(|terminal_id| app.terminals.get(terminal_id))
                .and_then(|terminal| terminal.ownership().detected_agent());
            detected.is_some_and(|agent| app.settings.cjk_ime_agents.contains(&agent))
        });

    if let Some(cursor) = runtime.read().cursor_state(area) {
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
                app.settings.cjk_ime_cursor_shape
            } else {
                cursor.shape
            },
        })
    } else if reveal && !scrolled_back {
        Some(CursorState {
            x: area.x,
            y: area.y,
            visible: true,
            shape: app.settings.cjk_ime_cursor_shape,
        })
    } else {
        None
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
        let left = workspace.root_pane();
        let right = workspace.test_split(Direction::Horizontal);
        let mut runtimes = PaneRuntimeRegistry::new();
        let left_terminal = workspace.terminal_id(left).cloned().expect("left terminal");
        let right_terminal = workspace
            .terminal_id(right)
            .cloned()
            .expect("right terminal");
        runtimes.insert(
            left_terminal,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
                20,
                8,
                format!("\x1b]8;;{uri}\x1b\\LEFT\x1b]8;;\x1b\\").as_bytes(),
            ),
        );
        runtimes.insert(
            right_terminal,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 8, b"RIGHT"),
        );

        let mut app = AppState::test_new();
        app.workspaces = vec![workspace];
        app.set_bookmark_index(Some(0));

        let full_area = Rect::new(0, 0, 106, 20);
        let area = full_area;
        let surface =
            compute_surface_for(&app, &runtimes, Some(app.workspaces[0].id.clone()), area);
        assert_eq!(surface.pane_infos.len(), 2);
        assert!(!surface.split_borders.is_empty());

        // The recorded layout area of the workspace is session geometry for spawn
        // sizing; a client surface is drawn from its own layout alone.
        app.test_record_all_workspace_areas(Rect::new(9, 8, 7, 6));

        let surface_view = SurfaceView {
            target: surface.target.as_ref(),
            pane_infos: &surface.pane_infos,
            split_borders: &surface.split_borders,
        };
        let mut frame = FrameData::blank(full_area.width, full_area.height);
        render_surface(&app, &runtimes, surface_view, &mut frame);

        let rendered = frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(rendered.contains("LEFT"), "surface: {rendered:?}");
        assert!(rendered.contains("RIGHT"), "surface: {rendered:?}");
        assert!(!rendered.contains("shell-workspace"));

        // The link is on the cells of the linked text, in the frame's table.
        assert_eq!(frame.hyperlinks, vec![uri.to_owned()]);
        let linked = frame
            .cells
            .iter()
            .filter(|cell| cell.hyperlink.is_some())
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert_eq!(linked, "LEFT");
        assert!(surface_cursor(&app, &runtimes, surface_view,).is_some());
    }
}
