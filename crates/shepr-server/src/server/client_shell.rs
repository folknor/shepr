use ratatui::layout::Rect;
use std::sync::Arc;

use crate::app;
use crate::server::clients::ClientPaneIdentity;
use shepr_protocol::FrameData;

pub(super) fn snapshot(
    app: &app::App,
    boot_id: &shepr_protocol::BootId,
    revision: u64,
    location: &crate::server::clients::ClientShellLocation,
) -> shepr_protocol::ClientShellSnapshot {
    snapshot_from_session(app, &app.session_snapshot(), boot_id, revision, location)
}

/// Projects an already built `app.session_snapshot()` for one shell
/// client.
///
/// The session snapshot underneath is cached by the headless server and shared
/// across clients. Borrowing it avoids cloning its source vectors before
/// building the owned client snapshot; fields carried onto the wire still need
/// their own owned values. Rendering re-projects it when the shared generation
/// moves, this client's location generation moves, or its snapshot is
/// missing. The shared generation advances after an application revision or
/// when the cwd timer finds a changed projection; a location change invalidates
/// only the client that moved.
pub(super) fn snapshot_from_session(
    app: &app::App,
    snapshot: &crate::app::SessionSnapshot,
    boot_id: &shepr_protocol::BootId,
    revision: u64,
    location: &crate::server::clients::ClientShellLocation,
) -> shepr_protocol::ClientShellSnapshot {
    // The client views what its own location names and nothing else: a client
    // with no workspace has no focus, never the session's bookmark.
    let focused_workspace_id = location
        .focused_workspace_id
        .clone()
        .filter(|workspace_id| app.resolve_workspace_id(workspace_id).is_some());
    let focused_pane_id = focused_workspace_id
        .as_ref()
        .and_then(|workspace_id| app.resolve_workspace_id(workspace_id))
        .and_then(|workspace_index| {
            let pane_id = app.state.workspaces.get(workspace_index)?.focused_pane_id();
            app.public_pane_id(workspace_index, pane_id)
        });
    // Snapshot entries are joined to live state by their public ids, never by
    // position: a snapshot that filtered or reordered entries would otherwise
    // hand one workspace's labels and branch to another. The snapshot is built
    // from this same `app`, so the positional slot is tried first and the id
    // lookup only runs when it does not match.
    let workspaces = snapshot
        .workspaces
        .iter()
        .enumerate()
        .map(|(position, workspace)| {
            let workspace_id = &workspace.workspace_id;
            let workspace_index = app
                .state
                .workspaces
                .get(position)
                .is_some_and(|state| &state.id == workspace_id)
                .then_some(position)
                .or_else(|| app.resolve_workspace_id(workspace_id));
            let state = workspace_index.and_then(|index| app.state.workspaces.get(index));
            let new_workspace_cwd = workspace_index.map_or_default(|workspace_index| {
                app.resolved_new_workspace_cwd(workspace_index)
                    .display()
                    .to_string()
            });
            shepr_protocol::ClientShellWorkspace {
                focused: focused_workspace_id.as_ref() == Some(workspace_id),
                workspace_id: workspace_id.clone(),
                new_workspace_cwd,
                number: workspace.number,
                label: workspace.label.clone(),
                custom_label: state.is_some_and(|state| state.custom_name.is_some()),
                branch: state.and_then(shepr_mux::workspace::Workspace::branch),
                git_ahead_behind: state
                    .and_then(shepr_mux::workspace::Workspace::git_ahead_behind)
                    .map(|counts| (counts.ahead, counts.behind)),
                agent_status: workspace.agent_status,
            }
        })
        .collect();
    let panes = snapshot
        .panes
        .iter()
        .map(|pane| {
            let focused = focused_pane_id.as_ref() == Some(&pane.pane_id);
            let right_click_passthrough = app
                .resolve_pane_id(&pane.pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    app.state
                        .workspaces
                        .get(workspace_index)?
                        .pane_state(pane_id)
                })
                .is_some_and(|pane| pane.right_click_passthrough);
            shepr_protocol::ClientShellPane {
                pane_id: pane.pane_id.clone(),
                workspace_id: pane.workspace_id.clone(),
                label: pane.label.clone(),
                cwd: pane.cwd.clone(),
                foreground_cwd: pane.foreground_cwd.clone(),
                focused,
                right_click_passthrough,
            }
        })
        .collect();
    let agents = snapshot
        .agents
        .iter()
        .map(|agent| {
            let focused = focused_pane_id.as_ref() == Some(&agent.pane_id);
            shepr_protocol::ClientShellAgent {
                pane_id: agent.pane_id.clone(),
                workspace_id: agent.workspace_id.clone(),
                agent: agent.agent.clone(),
                terminal_title: agent.terminal_title.clone(),
                terminal_title_stripped: agent.terminal_title_stripped.clone(),
                agent_status: agent.agent_status,
                state_change_seq: agent.state_change_seq,
                focused,
            }
        })
        .collect();

    shepr_protocol::ClientShellSnapshot {
        boot_id: boot_id.clone(),
        revision: revision.into(),
        focused_workspace_id,
        focused_pane_id,
        workspaces,
        panes,
        agents,
    }
}

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

pub(super) fn render_pane_surface(
    app: &app::App,
    target: Option<&shepr_protocol::WorkspaceId>,
    area: Rect,
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) -> Result<RenderedPaneSurface, SurfaceRenderDeferred> {
    let layout =
        crate::ui::compute_surface_for(&app.state, &app.terminal_runtimes, target.cloned(), area);
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
                let Some((synchronized, epoch)) = runtime.synchronized_output_state() else {
                    return Err(SurfaceRenderDeferred::Poisoned);
                };
                if synchronized {
                    return Err(SurfaceRenderDeferred::Synchronized);
                }
                let revision = runtime.content_seq();
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
            let mouse_reporting =
                runtime.is_some_and(shepr_mux::pane::PaneRuntime::mouse_reporting_enabled);
            let sgr_pixel_mouse =
                runtime.is_some_and(shepr_mux::pane::PaneRuntime::sgr_pixel_mouse_enabled);
            let (pixel_width, pixel_height) = if cell_size.is_known() {
                (
                    u32::from(pane.inner_rect.width) * cell_size.width_px,
                    u32::from(pane.inner_rect.height) * cell_size.height_px,
                )
            } else {
                (0, 0)
            };
            let content_revision = runtime.map_or(0, |runtime| {
                let after = runtime.content_seq();
                if content_revisions_before
                    .get(&pane.id)
                    .is_some_and(|&(_, before)| before == after)
                    && after.is_multiple_of(2)
                {
                    after
                } else {
                    after | 1
                }
            });
            panes.push(shepr_protocol::PaneSurfacePane {
                pane_id,
                content_revision,
                rect: pane.rect.into(),
                inner_rect: pane.inner_rect.into(),
                scrollbar_rect: pane.scrollbar_rect.map(Into::into),
                scroll: runtime
                    .and_then(shepr_mux::pane::PaneRuntime::scroll_metrics)
                    .map(|metrics| shepr_protocol::PaneSurfaceScrollMetrics {
                        offset_from_bottom: metrics.offset_from_bottom as u64,
                        max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                        viewport_rows: metrics.viewport_rows as u64,
                        history_origin: metrics.history_origin,
                    }),
                focused: pane.is_focused,
                mouse_reporting,
                sgr_pixel_mouse,
                alternate_screen_active: runtime
                    .is_some_and(shepr_mux::pane::PaneRuntime::alternate_screen_active),
                pixel_width,
                pixel_height,
            });
            pane_identities.push(ClientPaneIdentity {
                workspace_id: workspace_id.clone(),
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
            let hit_rect = split_hit_rect(
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
                let Some((synchronized, after_epoch)) = runtime.synchronized_output_state() else {
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

fn split_hit_rect(
    split: &shepr_core::layout::SplitBorder,
    pane_borders: bool,
    pane_gaps: bool,
    pane_frames: &[Rect],
) -> Option<Rect> {
    let hit = match (split.direction, pane_borders, pane_gaps) {
        (shepr_core::layout::Direction::Horizontal, true, false) => {
            Rect::new(split.pos, split.area.y, 1, split.area.height)
        }
        (shepr_core::layout::Direction::Horizontal, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                start,
                split.area.y,
                split.pos.saturating_sub(start).saturating_add(1),
                split.area.height,
            )
        }
        (shepr_core::layout::Direction::Horizontal, false, true) => Rect::new(
            split.pos.checked_sub(1)?,
            split.area.y,
            1,
            split.area.height,
        ),
        (shepr_core::layout::Direction::Vertical, true, false) => {
            Rect::new(split.area.x, split.pos, split.area.width, 1)
        }
        (shepr_core::layout::Direction::Vertical, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                split.area.x,
                start,
                split.area.width,
                split.pos.saturating_sub(start).saturating_add(1),
            )
        }
        (shepr_core::layout::Direction::Vertical, false, true) => {
            Rect::new(split.area.x, split.pos.checked_sub(1)?, split.area.width, 1)
        }
        (_, false, false) => return None,
    };
    if !pane_borders
        && pane_frames.iter().any(|pane| {
            hit.x < pane.right()
                && hit.right() > pane.x
                && hit.y < pane.bottom()
                && hit.bottom() > pane.y
        })
    {
        return None;
    }
    Some(hit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn snapshot_state_fields_follow_ids_not_positions() {
        let mut app = app::App::new(
            &shepr_config::Config::default(),
            app::AppPolicy::Test,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        let mut first = shepr_mux::workspace::Workspace::test_new("first");
        // `test_new` always sets a custom name for identification; clear it
        // so only `second` below is actually custom-named, which is what
        // this test's `custom_label` assertions check.
        first.custom_name = None;
        let mut second = shepr_mux::workspace::Workspace::test_new("second");
        second.custom_name = Some("named".into());
        app.state.workspaces = vec![first, second];
        app.state.ensure_test_terminals();

        let second_workspace_id = app.state.workspaces[1].id.clone();
        let snapshot = snapshot(
            &app,
            &shepr_test_fixtures::fixed_boot_id(1),
            1,
            &crate::server::clients::ClientShellLocation::default(),
        );

        for workspace in &snapshot.workspaces {
            assert_eq!(
                workspace.custom_label,
                workspace.workspace_id == second_workspace_id,
                "workspace {}",
                workspace.workspace_id
            );
        }
        assert_eq!(snapshot.workspaces.len(), 2);
    }

    #[test]
    fn split_hits_follow_released_border_and_gap_geometry() {
        let horizontal = shepr_core::layout::SplitBorder {
            pos: 20,
            direction: shepr_core::layout::Direction::Horizontal,
            ratio: 0.5,
            area: shepr_core::geometry::Rect::new(2, 3, 40, 12),
            path: vec![shepr_core::geometry::SplitBranch::First],
        };
        assert_eq!(
            split_hit_rect(&horizontal, true, false, &[]),
            Some(Rect::new(20, 3, 1, 12))
        );
        assert_eq!(
            split_hit_rect(&horizontal, true, true, &[]),
            Some(Rect::new(19, 3, 2, 12))
        );
        assert_eq!(
            split_hit_rect(&horizontal, false, true, &[]),
            Some(Rect::new(19, 3, 1, 12))
        );
        assert_eq!(split_hit_rect(&horizontal, false, false, &[]), None);

        let vertical = shepr_core::layout::SplitBorder {
            pos: 9,
            direction: shepr_core::layout::Direction::Vertical,
            ratio: 0.5,
            area: shepr_core::geometry::Rect::new(2, 3, 40, 12),
            path: vec![shepr_core::geometry::SplitBranch::Second],
        };
        assert_eq!(
            split_hit_rect(&vertical, true, true, &[]),
            Some(Rect::new(2, 8, 40, 2))
        );

        let edge = shepr_core::layout::SplitBorder {
            pos: 0,
            direction: shepr_core::layout::Direction::Horizontal,
            ratio: 0.5,
            area: shepr_core::geometry::Rect::new(0, 0, 1, 4),
            path: Vec::new(),
        };
        assert_eq!(
            split_hit_rect(&edge, true, true, &[]),
            Some(Rect::new(0, 0, 1, 4))
        );
        assert_eq!(split_hit_rect(&edge, false, true, &[]), None);
        assert_eq!(
            split_hit_rect(&horizontal, false, true, &[Rect::new(19, 3, 1, 12)]),
            None
        );
    }
}
