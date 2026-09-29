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

/// Marks the explicit geometry application paths that resize PTYs; drawing
/// functions take the registry instead. This separates the two in every
/// signature, but it is not a compile-time barrier: `PaneRuntime::resize`
/// takes `&self`, so code holding a runtime could still call it.
pub(crate) struct PaneResizer<'a> {
    runtimes: &'a PaneRuntimeRegistry,
}

impl<'a> PaneResizer<'a> {
    pub(crate) fn new(runtimes: &'a PaneRuntimeRegistry) -> Self {
        Self { runtimes }
    }

    fn runtime(
        &self,
        terminal_id: &shepr_protocol::TerminalId,
    ) -> Option<&shepr_mux::pane::PaneRuntime> {
        self.runtimes.get(terminal_id)
    }
}

/// Compute the active view geometry without resizing any terminal runtimes.
/// The caller stores the result in `AppState::view`; taking the state by
/// shared reference keeps every other field out of reach. The pane list is
/// built fresh by the tab surface layout either way, so returning it costs no
/// copy over an in-place update.
pub(crate) fn compute_view(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    area: Rect,
) -> crate::app::ViewState {
    let TabSurfaceLayout { pane_infos, .. } = compute_tab_surface(app, terminal_runtimes, area);

    crate::app::ViewState {
        terminal_area: area,
        pane_infos,
    }
}

/// Resize visible panes in every open tab using the supplied terminal area.
pub(crate) fn resize_all_tab_surfaces(
    app: &AppState,
    resizer: &PaneResizer<'_>,
    area: Rect,
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) {
    for (workspace_index, workspace) in app.workspaces.iter().enumerate() {
        for tab_index in 0..workspace.tabs().len() {
            resize_tab_surface(app, resizer, workspace_index, tab_index, area, cell_size);
        }
    }
}
