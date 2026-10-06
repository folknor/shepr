use std::sync::Arc;

use crate::app;
use crate::server::clients::ClientPaneIdentity;
use shepr_mux::pane::SyncState;
use shepr_protocol::FrameData;
use shepr_surface::ratatui_conversion::surface_rect;

#[derive(Clone)]
pub(super) struct RenderedPaneSurface {
    /// Shared by clients with the same workspace and geometry during one
    /// render pass. Each wire surface takes an owned frame at the boundary.
    pub(super) frame: Arc<FrameData>,
    pub(super) panes: Vec<shepr_protocol::PaneSurfacePane>,
    pub(super) splits: Vec<shepr_protocol::PaneSurfaceSplit>,
    /// Typed identities aligned with `panes` and retained beside each client's
    /// committed wire baseline for later render paths.
    pub(super) pane_identities: Vec<ClientPaneIdentity>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum SurfaceRenderDeferred {
    Synchronized,
    Changed,
    /// A pane's terminal core is poisoned. The PTY actor closes that pane
    /// shortly; until then the frame is deferred like a synchronized update.
    Poisoned,
    /// The client's area is outside the surface budget, so no frame can hold
    /// it. Client sizes are clamped into the budget before they reach a render,
    /// so this is a guard.
    Unrepresentable,
}

/// Runtime-dependent wire fields. Complete renders retain the before/after
/// content certificate; dirty snapshots already certify their collected rows.
#[derive(Clone, Copy, Default)]
pub(super) struct PaneSurfaceMetadata {
    content_revision: shepr_protocol::ContentRevision,
    scroll: Option<shepr_mux::pane::ScrollMetrics>,
    mouse_reporting: bool,
    pixel_mouse: shepr_term::mouse::PanePixelMouse,
    pub(super) alternate_screen_active: bool,
}

impl PaneSurfaceMetadata {
    fn from_runtime(
        runtime: &shepr_mux::pane::PaneRuntime,
        before: Option<shepr_mux::pane::ContentRevision>,
    ) -> Self {
        let read = runtime.read();
        Self {
            content_revision: shepr_mux::pane::PaneRuntime::surface_content_revision(
                before,
                read.content_revision(),
            ),
            scroll: read.scroll_metrics(),
            mouse_reporting: read.mouse_reporting_enabled(),
            pixel_mouse: read.pixel_mouse(),
            alternate_screen_active: read.alternate_screen_active(),
        }
    }

    pub(super) fn from_dirty_snapshot(
        snapshot: &shepr_mux::pane::TerminalDirtyPatchSnapshot,
    ) -> Self {
        Self {
            content_revision: snapshot.content_revision,
            scroll: Some(snapshot.scroll_metrics),
            mouse_reporting: snapshot.mouse_reporting,
            pixel_mouse: snapshot.pixel_mouse,
            alternate_screen_active: snapshot.alternate_screen_active,
        }
    }

    pub(super) fn scroll(&self) -> Option<shepr_mux::pane::ScrollMetrics> {
        self.scroll
    }

    pub(super) fn apply(self, pane: &mut shepr_protocol::PaneSurfacePane) {
        pane.content_revision = self.content_revision;
        pane.scroll = self.scroll;
        pane.mouse_reporting = self.mouse_reporting;
        pane.pixel_mouse = self.pixel_mouse;
        pane.alternate_screen_active = self.alternate_screen_active;
    }
}

pub(super) fn render_pane_surface(
    app: &app::App,
    target: Option<crate::ui::SurfaceTarget>,
    area: shepr_core::geometry::Rect,
) -> Result<RenderedPaneSurface, SurfaceRenderDeferred> {
    let area = crate::ui::ratatui_rect(area);
    let view = app.render_view();
    let layout = crate::ui::compute_surface_for(view.state, view.runtimes, target, area);
    if let Some(target) = target
        && target.workspace(view.state).is_none()
    {
        return Err(SurfaceRenderDeferred::Changed);
    }
    let Ok((frame, layout, draws)) = crate::server::render_stream::render_surface_virtual(
        view.state,
        view.runtimes,
        layout,
        area,
    ) else {
        return Err(SurfaceRenderDeferred::Unrepresentable);
    };
    // Each pane's draw says in its own core hold whether the screen was drawn,
    // and from which synchronized-output epoch and content revision. A pane
    // held back by a synchronized update or an unreadable core defers the
    // whole surface; there is no separate check before drawing.
    let mut content_revisions_before = std::collections::HashMap::new();
    for (pane_id, draw) in draws {
        match draw {
            shepr_mux::pane::PaneDraw::Unreadable => return Err(SurfaceRenderDeferred::Poisoned),
            shepr_mux::pane::PaneDraw::Deferred => return Err(SurfaceRenderDeferred::Synchronized),
            shepr_mux::pane::PaneDraw::Drawn {
                sync_epoch,
                content_revision,
            } => {
                content_revisions_before.insert(pane_id, (sync_epoch, Some(content_revision)));
            }
        }
    }
    let mut panes = Vec::new();
    let mut pane_identities = Vec::new();
    if let Some(workspace) = target.and_then(|target| target.workspace(view.state)) {
        let workspace_id = workspace.id();
        for pane in &layout.panes {
            let Some(record) = workspace.tree().pane(pane.id) else {
                continue;
            };
            let pane_id = shepr_protocol::PublicPaneId::new(&workspace_id, record.number());
            let runtime = app.pane_runtime(pane.id);
            let metadata = runtime.map_or_else(PaneSurfaceMetadata::default, |runtime| {
                PaneSurfaceMetadata::from_runtime(
                    runtime,
                    content_revisions_before
                        .get(&pane.id)
                        .and_then(|&(_, before)| before),
                )
            });
            let mut surface_pane = shepr_protocol::PaneSurfacePane {
                pane_id,
                content_revision: shepr_protocol::ContentRevision::default(),
                rect: surface_rect(pane.rect),
                content_rect: surface_rect(pane.content_rect),
                scrollbar_rect: pane.scrollbar_rect.map(surface_rect),
                scroll: None,
                focused: pane.is_focused,
                mouse_reporting: false,
                pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
                alternate_screen_active: false,
            };
            metadata.apply(&mut surface_pane);
            panes.push(surface_pane);
            pane_identities.push(ClientPaneIdentity {
                workspace_id,
                pane_id: pane.id,
            });
        }
    }
    let layout_epoch = target
        .and_then(|target| target.workspace(view.state))
        .map_or_default(|workspace| workspace.tree().layout_epoch());
    let splits = layout
        .split_borders
        .iter()
        .map(|split| {
            let hit_rect = crate::ui::split_hit_rect(split, view.state.settings().pane_gaps);
            shepr_protocol::PaneSurfaceSplit {
                direction: split.direction,
                pos: split.pos,
                area: split.area,
                hit_rect: surface_rect(hit_rect),
                path: split.path.branches().to_vec(),
                epoch: layout_epoch,
            }
        })
        .collect();
    if let Some(target) = target {
        if target.workspace(view.state).is_none() {
            return Err(SurfaceRenderDeferred::Changed);
        }
        for (&pane_id, &(epoch, _)) in &content_revisions_before {
            if let Some(runtime) = app.pane_runtime(pane_id) {
                match runtime.read().synchronized_output_state() {
                    SyncState::Poisoned => return Err(SurfaceRenderDeferred::Poisoned),
                    SyncState::Active => return Err(SurfaceRenderDeferred::Synchronized),
                    SyncState::Idle(after_epoch) if after_epoch != epoch => {
                        return Err(SurfaceRenderDeferred::Changed);
                    }
                    SyncState::Idle(_) => {}
                }
            }
        }
    }
    Ok(RenderedPaneSurface {
        frame: Arc::new(frame),
        panes,
        splits,
        pane_identities,
    })
}
