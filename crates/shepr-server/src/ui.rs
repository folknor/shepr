mod chrome;
mod pane_surface;
mod panes;
mod scrollbar;
mod surface;
mod text;

pub(crate) use self::pane_surface::{PaneSurface, core_rect, ratatui_rect};
pub(crate) use self::panes::split_hit_rect;
pub(crate) use self::surface::{
    PaneLayoutCache, SurfaceLayout, SurfaceTarget, SurfaceView, compute_pane_surfaces,
    compute_surface_for, render_surface, surface_cursor,
};
