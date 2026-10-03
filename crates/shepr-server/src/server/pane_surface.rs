use ratatui::layout::Rect;
use std::sync::Arc;

use crate::app;
use crate::server::clients::ClientPaneIdentity;
use shepr_protocol::FrameData;

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
}

/// Runtime-dependent wire fields. Complete renders retain the before/after
/// content certificate; dirty snapshots already certify their collected rows.
#[derive(Clone, Copy, Default)]
pub(super) struct PaneSurfaceMetadata {
    content_revision: u64,
    scroll: Option<shepr_mux::pane::ScrollMetrics>,
    mouse_reporting: bool,
    sgr_pixel_mouse: bool,
    pub(super) alternate_screen_active: bool,
}

impl PaneSurfaceMetadata {
    fn from_runtime(runtime: &shepr_mux::pane::PaneRuntime, before: Option<u64>) -> Self {
        let read = runtime.read();
        Self {
            content_revision: shepr_mux::pane::PaneRuntime::surface_content_revision(
                before,
                read.content_seq(),
            ),
            scroll: read.scroll_metrics(),
            mouse_reporting: read.mouse_reporting_enabled(),
            sgr_pixel_mouse: read.sgr_pixel_mouse_enabled(),
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
            sgr_pixel_mouse: snapshot.sgr_pixel_mouse,
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
        pane.sgr_pixel_mouse = self.sgr_pixel_mouse;
        pane.alternate_screen_active = self.alternate_screen_active;
    }
}

pub(super) fn render_pane_surface(
    app: &app::App,
    target: Option<&shepr_protocol::WorkspaceId>,
    area: Rect,
    cell_size: shepr_term::host::HostCellSize,
) -> Result<RenderedPaneSurface, SurfaceRenderDeferred> {
    let layout =
        crate::ui::compute_surface_for(&app.state, &app.terminal_runtimes, target.copied(), area);
    let mut content_revisions_before = std::collections::HashMap::new();
    if let Some(target) = &target {
        let Some(workspace_index) = app.state.workspace_index(target) else {
            return Err(SurfaceRenderDeferred::Changed);
        };
        for pane in &layout.pane_infos {
            if let Some(runtime) = app.state.runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                workspace_index,
                pane.id,
            ) {
                let Some((synchronized, epoch)) = runtime.read().synchronized_output_state() else {
                    return Err(SurfaceRenderDeferred::Poisoned);
                };
                if synchronized {
                    return Err(SurfaceRenderDeferred::Synchronized);
                }
                let revision = runtime.read().content_seq();
                content_revisions_before.insert(pane.id, (epoch, revision));
            }
        }
    }
    let (frame, layout) = crate::server::render_stream::render_surface_virtual(
        &app.state,
        &app.terminal_runtimes,
        layout,
        area,
    );
    let mut panes = Vec::new();
    let mut pane_identities = Vec::new();
    if let Some(workspace_index) = target.and_then(|target| app.state.workspace_index(target)) {
        let Some(workspace_id) = app.public_workspace_id(workspace_index) else {
            return Err(SurfaceRenderDeferred::Changed);
        };
        for pane in &layout.pane_infos {
            let Some(pane_id) = app.public_pane_id(workspace_index, pane.id) else {
                continue;
            };
            let runtime = app.state.runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                workspace_index,
                pane.id,
            );
            let (pixel_width, pixel_height) = if cell_size.is_known() {
                (
                    u32::from(pane.inner_rect.width) * cell_size.width_px,
                    u32::from(pane.inner_rect.height) * cell_size.height_px,
                )
            } else {
                (0, 0)
            };
            let metadata = runtime.map_or_else(PaneSurfaceMetadata::default, |runtime| {
                PaneSurfaceMetadata::from_runtime(
                    runtime,
                    content_revisions_before
                        .get(&pane.id)
                        .map(|&(_, before)| before),
                )
            });
            let mut surface_pane = shepr_protocol::PaneSurfacePane {
                pane_id,
                content_revision: 0,
                rect: pane.rect.into(),
                inner_rect: pane.inner_rect.into(),
                scrollbar_rect: pane.scrollbar_rect.map(Into::into),
                scroll: None,
                focused: pane.is_focused,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width,
                pixel_height,
            };
            metadata.apply(&mut surface_pane);
            panes.push(surface_pane);
            pane_identities.push(ClientPaneIdentity {
                workspace_id,
                pane_id: pane.id,
            });
        }
    }
    let pane_frames = layout
        .pane_infos
        .iter()
        .map(|pane| pane.rect)
        .collect::<Vec<_>>();
    let splits = layout
        .split_borders
        .iter()
        .filter_map(|split| {
            let hit_rect = crate::ui::split_hit_rect(
                split,
                app.state.settings.pane_borders.draws_borders(),
                app.state.settings.pane_gaps,
                &pane_frames,
            )?;
            Some(shepr_protocol::PaneSurfaceSplit {
                direction: split.direction.into(),
                pos: split.pos,
                area: split.area.into(),
                hit_rect: hit_rect.into(),
                path: split.path.clone(),
            })
        })
        .collect();
    if let Some(target) = &target {
        let Some(workspace_index) = app.state.workspace_index(target) else {
            return Err(SurfaceRenderDeferred::Changed);
        };
        for (&pane_id, &(epoch, _)) in &content_revisions_before {
            if let Some(runtime) = app.state.runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                workspace_index,
                pane_id,
            ) {
                let Some((synchronized, after_epoch)) = runtime.read().synchronized_output_state()
                else {
                    return Err(SurfaceRenderDeferred::Poisoned);
                };
                if synchronized {
                    return Err(SurfaceRenderDeferred::Synchronized);
                }
                if after_epoch != epoch {
                    return Err(SurfaceRenderDeferred::Changed);
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
