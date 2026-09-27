use ratatui::layout::Rect;

use crate::app;
use shepr_protocol::FrameData;

pub(super) fn snapshot(
    app: &app::App,
    resolved_config: &[u8],
    boot_id: &str,
    revision: u64,
    location: Option<&crate::server::clients::ClientShellLocation>,
) -> shepr_protocol::ClientShellSnapshot {
    snapshot_from_session(
        app,
        app.session_snapshot(),
        resolved_config,
        boot_id,
        revision,
        location,
    )
}

/// Projects an already built `app.session_snapshot()` for one shell client.
///
/// A full render diffs every shell client's projection against what it was
/// last sent. The session snapshot underneath is the same for all of them
/// (only `location` is per client), so the render builds it once and hands
/// each client its own copy instead of rebuilding the whole session per
/// client.
pub(super) fn snapshot_from_session(
    app: &app::App,
    snapshot: shepr_api::schema::SessionSnapshot,
    resolved_config: &[u8],
    boot_id: &str,
    revision: u64,
    location: Option<&crate::server::clients::ClientShellLocation>,
) -> shepr_protocol::ClientShellSnapshot {
    let focused_workspace_id = location
        .and_then(|location| location.focused_workspace_id.clone())
        .or_else(|| snapshot.focused_workspace_id.clone().map(Into::into));
    let focused_tab_id = location
        .and_then(|location| location.focused_tab_id().cloned())
        .or_else(|| {
            snapshot
                .focused_tab_id
                .as_deref()
                .and_then(|id| id.parse().ok())
        });
    let focused_pane_id = focused_tab_id
        .as_deref()
        .and_then(|tab_id| app.parse_tab_id(tab_id))
        .and_then(|(workspace_index, tab_index)| {
            let pane_id = app
                .state
                .workspaces
                .get(workspace_index)?
                .tabs
                .get(tab_index)?
                .layout
                .focused();
            app.public_pane_id(workspace_index, pane_id)
                .and_then(|id| id.parse().ok())
        })
        .or_else(|| {
            snapshot
                .focused_pane_id
                .as_deref()
                .and_then(|id| id.parse().ok())
        });
    // Snapshot entries are joined to live state by their public ids, never by
    // position: a snapshot that filtered or reordered entries would otherwise
    // hand one workspace's or tab's labels, branch and zoom to another. The
    // snapshot is built from this same `app`, so the positional slot is tried
    // first and the id lookup only runs when it does not match.
    let workspaces = snapshot
        .workspaces
        .into_iter()
        .enumerate()
        .filter_map(|(position, workspace)| {
            let mut tokens = workspace.tokens.into_iter().collect::<Vec<_>>();
            tokens.sort_by(|left, right| left.0.cmp(&right.0));
            let workspace_id = workspace.workspace_id;
            let workspace_index = app
                .state
                .workspaces
                .get(position)
                .is_some_and(|state| state.id == workspace_id)
                .then_some(position)
                .or_else(|| app.parse_workspace_id(&workspace_id));
            let state = workspace_index.and_then(|index| app.state.workspaces.get(index));
            let active_tab_id = location
                .and_then(|location| {
                    location
                        .active_tab_ids
                        .get(&shepr_protocol::WorkspaceId::new(workspace_id.as_str()))
                })
                .cloned()
                .or_else(|| workspace.active_tab_id.parse().ok())?;
            let new_workspace_cwd = workspace_index
                .map(|workspace_index| {
                    let active_tab_index = app.parse_tab_id(&active_tab_id).and_then(
                        |(tab_workspace_index, tab_index)| {
                            (tab_workspace_index == workspace_index).then_some(tab_index)
                        },
                    );
                    app.resolved_new_workspace_cwd_from_tab(workspace_index, active_tab_index)
                        .display()
                        .to_string()
                })
                .unwrap_or_default();
            Some(shepr_protocol::ClientShellWorkspace {
                focused: focused_workspace_id.as_deref() == Some(workspace_id.as_str()),
                workspace_id: workspace_id.into(),
                active_tab_id,
                new_workspace_cwd,
                number: workspace.number,
                label: workspace.label,
                custom_label: state.is_some_and(|state| state.custom_name.is_some()),
                branch: state.and_then(shepr_mux::workspace::Workspace::branch),
                git_ahead_behind: state
                    .and_then(shepr_mux::workspace::Workspace::git_ahead_behind)
                    .map(|counts| (counts.ahead, counts.behind)),
                tokens,
                agent_status: workspace.agent_status,
            })
        })
        .collect();
    let tabs = snapshot
        .tabs
        .into_iter()
        .filter_map(|tab| {
            let tab_id: shepr_protocol::PublicTabId = tab.tab_id.parse().ok()?;
            let state = app
                .parse_tab_id(&tab_id)
                .and_then(|(workspace_index, tab_index)| {
                    app.state
                        .workspaces
                        .get(workspace_index)?
                        .tabs
                        .get(tab_index)
                });
            Some(shepr_protocol::ClientShellTab {
                focused: focused_tab_id.as_deref() == Some(tab_id.as_str()),
                tab_id,
                workspace_id: tab.workspace_id.into(),
                number: tab.number,
                label: tab.label,
                custom_label: state.is_some_and(|state| !state.is_auto_named()),
                zoomed: state.is_some_and(|state| state.zoomed),
                agent_status: tab.agent_status,
            })
        })
        .collect();
    let panes = snapshot
        .panes
        .into_iter()
        .filter_map(|pane| {
            let pane_id: shepr_protocol::PublicPaneId = pane.pane_id.parse().ok()?;
            let focused = focused_pane_id.as_deref() == Some(pane_id.as_str());
            let right_click_passthrough = app
                .parse_pane_id(&pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    app.state
                        .workspaces
                        .get(workspace_index)?
                        .pane_state(pane_id)
                })
                .is_some_and(|pane| pane.right_click_passthrough);
            Some(shepr_protocol::ClientShellPane {
                pane_id,
                workspace_id: pane.workspace_id.into(),
                tab_id: pane.tab_id.parse().ok()?,
                label: pane.label,
                cwd: pane.cwd,
                foreground_cwd: pane.foreground_cwd,
                focused,
                right_click_passthrough,
            })
        })
        .collect();
    let agents = snapshot
        .agents
        .into_iter()
        .filter_map(|agent| {
            let focused = focused_pane_id.as_deref() == Some(agent.pane_id.as_str());
            let pane_id = agent.pane_id.parse().ok()?;
            let mut state_labels = agent.state_labels.into_iter().collect::<Vec<_>>();
            state_labels.sort_by(|left, right| left.0.cmp(&right.0));
            let mut tokens = agent.tokens.into_iter().collect::<Vec<_>>();
            tokens.sort_by(|left, right| left.0.cmp(&right.0));
            Some(shepr_protocol::ClientShellAgent {
                pane_id,
                workspace_id: agent.workspace_id.into(),
                tab_id: agent.tab_id.parse().ok()?,
                name: agent.name,
                display_agent: agent.display_agent,
                agent: agent.agent,
                title: agent.title,
                terminal_title: agent.terminal_title,
                terminal_title_stripped: agent.terminal_title_stripped,
                agent_status: agent.agent_status,
                state_change_seq: agent.state_change_seq,
                state_labels,
                tokens,
                focused,
            })
        })
        .collect();

    let zoomed = focused_tab_id
        .as_deref()
        .and_then(|tab_id| app.parse_tab_id(tab_id))
        .and_then(|(workspace_index, tab_index)| {
            app.state
                .workspaces
                .get(workspace_index)?
                .tabs
                .get(tab_index)
        })
        .is_some_and(|tab| tab.zoomed);
    let tab_bar_right = app
        .state
        .tab_bar_right
        .iter()
        .filter_map(|segment| match segment {
            crate::app::state::TabBarStatusSegment::Zoom if zoomed => {
                Some(shepr_protocol::ClientShellTabStatusSegment {
                    text: "ZOOM".to_owned(),
                    accent: true,
                })
            }
            crate::app::state::TabBarStatusSegment::Text(Some(text)) if !text.is_empty() => {
                Some(shepr_protocol::ClientShellTabStatusSegment {
                    text: text.clone(),
                    accent: false,
                })
            }
            crate::app::state::TabBarStatusSegment::Zoom
            | crate::app::state::TabBarStatusSegment::Text(_) => None,
        })
        .collect();

    shepr_protocol::ClientShellSnapshot {
        boot_id: boot_id.into(),
        revision: revision.into(),
        resolved_config: resolved_config.to_vec(),
        focused_workspace_id,
        focused_tab_id,
        focused_pane_id,
        tab_bar_right,
        tab_bar_right_separator: app.state.tab_bar_right_separator.clone(),
        workspaces,
        tabs,
        panes,
        agents,
    }
}

pub(super) struct RenderedPaneSurface {
    pub(super) frame: FrameData,
    pub(super) panes: Vec<shepr_protocol::PaneSurfacePane>,
    pub(super) splits: Vec<shepr_protocol::PaneSurfaceSplit>,
}

#[derive(Debug)]
pub(super) enum SurfaceRenderDeferred {
    Synchronized,
    Changed,
}

pub(super) fn render_pane_surface(
    app: &app::App,
    target: Option<&crate::ui::TabSurfaceTarget>,
    area: Rect,
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) -> Result<RenderedPaneSurface, SurfaceRenderDeferred> {
    let layout = crate::ui::compute_tab_surface_for(
        &app.state,
        &app.terminal_runtimes,
        target.cloned(),
        area,
    );
    let mut content_revisions_before = std::collections::HashMap::new();
    if let Some(target) = &target {
        let Some((workspace_index, _)) = target.resolve(&app.state) else {
            return Err(SurfaceRenderDeferred::Changed);
        };
        for pane in &layout.pane_infos {
            if let Some(runtime) = app.state.runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                workspace_index,
                pane.id,
            ) {
                let (synchronized, epoch) = runtime.synchronized_output_state();
                if synchronized {
                    return Err(SurfaceRenderDeferred::Synchronized);
                }
                let revision = runtime.content_seq();
                content_revisions_before.insert(pane.id, (epoch, revision));
            }
        }
    }
    let (buffer, cursor, hyperlinks, layout) =
        crate::server::render_stream::render_tab_surface_virtual(
            &app.state,
            &app.terminal_runtimes,
            layout,
            area,
        );
    let panes = target
        .as_ref()
        .and_then(|target| target.resolve(&app.state))
        .map(|(workspace_index, _)| {
            layout
                .pane_infos
                .iter()
                .filter_map(|pane| {
                    app.public_pane_id(workspace_index, pane.id)
                        .and_then(|pane_id| {
                            let pane_id = pane_id.parse().ok()?;
                            let runtime = app.state.runtime_for_pane_in_workspace(
                                &app.terminal_runtimes,
                                workspace_index,
                                pane.id,
                            );
                            let mouse_reporting = runtime
                                .is_some_and(shepr_mux::pane::PaneRuntime::mouse_reporting_enabled);
                            let sgr_pixel_mouse = runtime
                                .is_some_and(shepr_mux::pane::PaneRuntime::sgr_pixel_mouse_enabled);
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
                            Some(shepr_protocol::PaneSurfacePane {
                                pane_id,
                                content_revision,
                                rect: pane.rect.into(),
                                inner_rect: pane.inner_rect.into(),
                                scrollbar_rect: pane.scrollbar_rect.map(Into::into),
                                scroll: runtime
                                    .and_then(shepr_mux::pane::PaneRuntime::scroll_metrics)
                                    .map(|metrics| shepr_protocol::PaneSurfaceScrollMetrics {
                                        offset_from_bottom: metrics.offset_from_bottom as u64,
                                        max_offset_from_bottom: metrics.max_offset_from_bottom
                                            as u64,
                                        viewport_rows: metrics.viewport_rows as u64,
                                        history_origin: metrics.history_origin,
                                    }),
                                focused: pane.is_focused,
                                mouse_reporting,
                                sgr_pixel_mouse,
                                alternate_screen_active: runtime.is_some_and(
                                    shepr_mux::pane::PaneRuntime::alternate_screen_active,
                                ),
                                pixel_width,
                                pixel_height,
                            })
                        })
                })
                .collect()
        })
        .unwrap_or_default();
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
            let direction = match split.direction {
                ratatui::layout::Direction::Horizontal => {
                    shepr_protocol::PaneSurfaceSplitDirection::Horizontal
                }
                ratatui::layout::Direction::Vertical => {
                    shepr_protocol::PaneSurfaceSplitDirection::Vertical
                }
            };
            Some(shepr_protocol::PaneSurfaceSplit {
                direction,
                pos: split.pos,
                area: split.area.into(),
                hit_rect: hit_rect.into(),
                path: split.path.clone(),
            })
        })
        .collect();
    if let Some(target) = &target {
        let Some((workspace_index, _)) = target.resolve(&app.state) else {
            return Err(SurfaceRenderDeferred::Changed);
        };
        for (&pane_id, &(epoch, _)) in &content_revisions_before {
            if let Some(runtime) = app.state.runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                workspace_index,
                pane_id,
            ) {
                let (synchronized, after_epoch) = runtime.synchronized_output_state();
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
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks),
        panes,
        splits,
    })
}

fn split_hit_rect(
    split: &shepr_core::layout::SplitBorder,
    pane_borders: bool,
    pane_gaps: bool,
    pane_frames: &[Rect],
) -> Option<Rect> {
    let hit = match (split.direction, pane_borders, pane_gaps) {
        (ratatui::layout::Direction::Horizontal, true, false) => {
            Rect::new(split.pos, split.area.y, 1, split.area.height)
        }
        (ratatui::layout::Direction::Horizontal, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                start,
                split.area.y,
                split.pos.saturating_sub(start).saturating_add(1),
                split.area.height,
            )
        }
        (ratatui::layout::Direction::Horizontal, false, true) => Rect::new(
            split.pos.checked_sub(1)?,
            split.area.y,
            1,
            split.area.height,
        ),
        (ratatui::layout::Direction::Vertical, true, false) => {
            Rect::new(split.area.x, split.pos, split.area.width, 1)
        }
        (ratatui::layout::Direction::Vertical, true, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                split.area.x,
                start,
                split.area.width,
                split.pos.saturating_sub(start).saturating_add(1),
            )
        }
        (ratatui::layout::Direction::Vertical, false, true) => {
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

    #[test]
    fn snapshot_state_fields_follow_ids_not_positions() {
        let mut app = app::App::new(
            &shepr_config::Config::default(),
            app::AppPolicy::TEST,
            tokio::sync::mpsc::unbounded_channel().1,
            shepr_api::EventHub::default(),
        );
        let mut first = shepr_mux::workspace::Workspace::test_new("first");
        // `test_new` always sets a custom name for identification; clear it
        // so only `second` below is actually custom-named, which is what
        // this test's `custom_label` assertions check.
        first.custom_name = None;
        first.test_add_tab(Some("second-tab"));
        let mut second = shepr_mux::workspace::Workspace::test_new("second");
        second.custom_name = Some("named".into());
        second.tabs[0].zoomed = true;
        app.state.workspaces = vec![first, second];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));

        let second_workspace_id = app.state.workspaces[1].id.clone();
        let zoomed_tab_id = app.public_tab_id(1, 0).expect("zoomed tab id");
        let resolved_config =
            shepr_protocol::codec::to_vec(&shepr_config::ValidatedConfig::test_default())
                .expect("encode test config");
        let snapshot = snapshot(&app, &resolved_config, "boot", 1, None);

        for workspace in &snapshot.workspaces {
            assert_eq!(
                workspace.custom_label,
                workspace.workspace_id == second_workspace_id,
                "workspace {}",
                workspace.workspace_id
            );
        }
        assert_eq!(snapshot.tabs.len(), 3);
        for tab in &snapshot.tabs {
            assert_eq!(
                tab.zoomed,
                tab.tab_id == zoomed_tab_id,
                "tab {}",
                tab.tab_id
            );
        }
    }

    #[test]
    fn split_hits_follow_released_border_and_gap_geometry() {
        let horizontal = shepr_core::layout::SplitBorder {
            pos: 20,
            direction: ratatui::layout::Direction::Horizontal,
            ratio: 0.5,
            area: Rect::new(2, 3, 40, 12),
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
            direction: ratatui::layout::Direction::Vertical,
            ratio: 0.5,
            area: Rect::new(2, 3, 40, 12),
            path: vec![shepr_core::geometry::SplitBranch::Second],
        };
        assert_eq!(
            split_hit_rect(&vertical, true, true, &[]),
            Some(Rect::new(2, 8, 40, 2))
        );

        let edge = shepr_core::layout::SplitBorder {
            pos: 0,
            direction: ratatui::layout::Direction::Horizontal,
            ratio: 0.5,
            area: Rect::new(0, 0, 1, 4),
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
