mod panes;
mod scrollbar;
mod tab_surface;
mod text;

pub(crate) use self::panes::pane_is_scrolled_back;
pub(crate) use self::scrollbar::render_pane_scrollbar_buffer;
pub(crate) use self::tab_surface::{
    TabSurfaceLayout, TabSurfaceTarget, TabSurfaceView, compute_tab_surface_for,
    render_tab_surface, resize_tab_surface, tab_surface_cursor, tab_surface_hyperlinks,
};

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
