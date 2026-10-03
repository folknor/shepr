mod chrome;
mod panes;
mod scrollbar;
mod surface;
mod text;

pub(crate) use self::panes::pane_is_scrolled_back;
pub(crate) use self::scrollbar::render_pane_scrollbar_buffer;
pub(crate) use self::surface::{
    SurfaceLayout, SurfaceView, compute_surface_for, render_surface, resize_surface, surface_cursor,
};

use shepr_mux::pane::PaneRuntimeRegistry;

/// Exclusive access for explicit geometry application. Drawing holds shared
/// access to the registry and cannot resize a runtime.
pub(crate) struct PaneResizer<'a> {
    runtimes: &'a mut PaneRuntimeRegistry,
}

impl<'a> PaneResizer<'a> {
    pub(crate) fn new(runtimes: &'a mut PaneRuntimeRegistry) -> Self {
        Self { runtimes }
    }

    fn runtime(
        &mut self,
        terminal_id: &shepr_protocol::TerminalId,
    ) -> Option<&mut shepr_mux::pane::PaneRuntime> {
        self.runtimes.get_mut(terminal_id)
    }
}
