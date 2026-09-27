use ratatui::layout::Rect;

mod panes;
mod scrollbar;
mod tab_surface;
mod text;

pub(crate) use self::panes::pane_is_scrolled_back;
pub(crate) use self::scrollbar::render_pane_scrollbar_buffer;
pub(crate) use self::tab_surface::{
    TabSurfaceLayout, TabSurfaceTarget, TabSurfaceView, compute_tab_surface,
    compute_tab_surface_for, render_tab_surface, resize_tab_surface, resize_tab_surface_layout,
    tab_surface_cursor, tab_surface_hyperlinks,
};

use crate::app::AppState;
use shepr_mux::pane::PaneRuntimeRegistry;

/// Refresh the active view geometry without resizing any terminal runtimes.
pub fn compute_view(app: &mut AppState, terminal_runtimes: &PaneRuntimeRegistry, area: Rect) {
    let TabSurfaceLayout { pane_infos, .. } = compute_tab_surface(app, terminal_runtimes, area);

    app.view = crate::app::ViewState {
        terminal_area: area,
        pane_infos,
    };
}

/// Resize visible panes in every open tab using the supplied terminal area.
pub(crate) fn resize_all_tab_surfaces(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    area: Rect,
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) {
    for (workspace_index, workspace) in app.workspaces.iter().enumerate() {
        for tab_index in 0..workspace.tabs().len() {
            resize_tab_surface(
                app,
                terminal_runtimes,
                workspace_index,
                tab_index,
                area,
                cell_size,
            );
        }
    }
}
