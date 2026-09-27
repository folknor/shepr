use super::*;

/// Cell range of a one-row `bar` inside `frame`, or `None` when any of it lies outside.
fn mode_bar_range(frame: &FrameData, bar: Rect) -> Option<std::ops::Range<usize>> {
    if bar.y >= frame.height || bar.right() > frame.width {
        return None;
    }
    let start = usize::from(bar.y) * usize::from(frame.width) + usize::from(bar.x);
    let end = start + usize::from(bar.width);
    (end <= frame.cells.len()).then_some(start..end)
}

fn restore_mode_bar(
    frame: &mut FrameData,
    bar: Option<Rect>,
    cells: Option<&[crate::protocol::CellData]>,
) {
    let (Some(bar), Some(cells)) = (bar, cells) else {
        return;
    };
    let Some(range) = mode_bar_range(frame, bar).filter(|range| range.len() == cells.len()) else {
        return;
    };
    frame.cells[range].clone_from_slice(cells);
    if frame
        .cursor
        .as_ref()
        .is_some_and(|cursor| cursor.y == bar.y)
    {
        frame.cursor = None;
    }
}

impl ClientShellState {
    fn compose_unavailable(&mut self, cols: u16, rows: u16) -> FrameData {
        let layout = self.layout(cols, rows);
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        buffer.set_style(
            buffer.area,
            Style::default()
                .fg(self.config.palette.text)
                .bg(self.config.palette.panel_bg),
        );
        self.hits = ShellHitMap::default();
        let sidebar = if layout.sidebar.width > 0 {
            layout.sidebar
        } else {
            Rect::new(0, 1, cols, rows.saturating_sub(2))
        };
        let valid_navigation_target = self.mode == ClientShellMode::Navigate
            && self
                .navigate_workspace_id
                .as_ref()
                .is_some_and(|target| self.navigation_target_valid(target));
        let pending_workspace_highlight =
            self.pending_workspace_highlight.as_ref().filter(|pending| {
                self.mode != ClientShellMode::Navigate
                    && pending.target.endpoint_id == self.active_endpoint_id
                    && self.navigation_target_valid(&pending.target)
            });
        // No pane surface yet (a fresh connection or projection) says nothing against a
        // healthy Local's workspace chrome; keep it rather than the machine list.
        let local_snapshot = self.snapshot.as_deref().filter(|_| {
            self.endpoints.len() == 1
                && !self.sidebar_collapsed
                && layout.sidebar.width > 0
                && self.endpoint_status(&self.active_endpoint_id)
                    == Some(ClientEndpointStatus::Online)
        });
        let mut render_state = render::ShellRenderState {
            machine_diagnostics: &self.machine_diagnostics,
            endpoints: &self.endpoints,
            active_endpoint_id: &self.active_endpoint_id,
            collapsed_endpoints: &self.collapsed_endpoints,
            workspace_scroll: &mut self.workspace_scroll,
            agent_scroll: &mut self.agent_scroll,
            tab_scroll: &mut self.tab_scroll,
            reveal_focused_workspace: &mut self.reveal_focused_workspace,
            reveal_focused_tab: &mut self.reveal_focused_tab,
            sidebar_collapsed: false,
            sidebar_section_split: self.sidebar_section_split,
            tab_drag_insert_index: None,
            selected_workspace_id: self
                .navigate_workspace_id
                .as_ref()
                .filter(|_| valid_navigation_target)
                .or_else(|| pending_workspace_highlight.map(|pending| &pending.target)),
            reveal_navigation_workspace: &mut self.reveal_navigation_workspace,
            dragged_workspace_id: None,
            workspace_drop_indicator_row: None,
        };
        super::endpoint_sidebar::render_expanded(
            &mut buffer,
            sidebar,
            local_snapshot.or(self.snapshot.as_deref()),
            &self.config,
            &mut render_state,
            &mut self.hits,
        );
        if !self.config.mouse_capture {
            self.hits = ShellHitMap::default();
        }
        let message = self.endpoint_error.clone().unwrap_or_else(|| {
            let status = self
                .endpoint_status(&self.active_endpoint_id)
                .unwrap_or(ClientEndpointStatus::Connecting);
            let (_, label, _) = endpoint_status_presentation(status, &self.config.palette);
            format!(
                "{}: {label}. Select a connected machine.",
                self.active_endpoint_label()
            )
        });
        let message_area = if layout.sidebar.width > 0 {
            layout.pane_surface
        } else {
            Rect::new(0, 0, cols, 1)
        };
        if local_snapshot.is_none() || self.endpoint_error.is_some() {
            render::put_text(
                &mut buffer,
                message_area.x,
                message_area.y,
                message_area.width,
                &message,
                Style::default().fg(self.config.palette.overlay0),
            );
        }
        render::render_mode_bar(
            &mut buffer,
            Rect::new(0, 0, cols, rows),
            self.mode,
            None,
            self.endpoint_error.as_deref(),
            &self.config.keybinds,
            &self.config.palette,
        );
        if let Some(notice) = &self.visible_endpoint_notice {
            self.hits.notification_toast = endpoint_notices::render_notice(
                &mut buffer,
                Rect::new(0, 0, cols, rows),
                notice,
                1,
                &self.config.palette,
            );
            self.endpoint_notice_drawn(std::time::Instant::now());
        }
        FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
    }

    pub(crate) fn compose(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> Option<crate::client::frame_output::ComposedFrame> {
        self.last_composed_at = Some(std::time::Instant::now());
        self.selection_repaint_deadline = None;
        if self.last_composed_size != Some((cols, rows)) && self.mode == ClientShellMode::Navigate {
            self.reveal_navigation_workspace = true;
        }
        self.last_composed_size = Some((cols, rows));
        let valid_navigation_target = self.mode == ClientShellMode::Navigate
            && self
                .navigate_workspace_id
                .as_ref()
                .is_some_and(|target| self.navigation_target_valid(target));
        let pending_workspace_highlight =
            self.pending_workspace_highlight.as_ref().filter(|pending| {
                self.mode != ClientShellMode::Navigate
                    && pending.target.endpoint_id == self.active_endpoint_id
                    && self.navigation_target_valid(&pending.target)
            });
        if self.snapshot.is_none() || self.pane_surface.is_none() {
            return Some(self.compose_unavailable(cols, rows).into());
        }
        let snapshot = self.snapshot.as_deref()?;
        // Do not compose a retained surface while waiting for its matching snapshot or
        // connection generation.
        if self.pending_pane_surface.is_some()
            || self.pane_surface_generation != self.active_snapshot_generation
        {
            return None;
        }
        let surface = self.pane_surface.as_ref()?;
        if snapshot.revision != surface.projection_revision {
            return None;
        }
        let layout = self.layout(cols, rows);
        if self.last_tab_bar_width != Some(layout.tab_bar.width) {
            self.last_tab_bar_width = Some(layout.tab_bar.width);
            self.reveal_focused_tab = true;
        }
        let tab_drag_insert_index = match &self.chrome_drag {
            Some(ClientChromeDrag::Tab { insert_index, .. }) => *insert_index,
            _ => None,
        };
        let (dragged_workspace_id, workspace_drop_indicator_row) = match &self.chrome_drag {
            Some(ClientChromeDrag::Workspace {
                source_workspace_id,
                target,
            }) => (
                Some(source_workspace_id),
                target.as_ref().map(|(_, row)| *row),
            ),
            _ => (None, None),
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        self.hits = render::render_shell(
            &mut buffer,
            layout,
            snapshot,
            &self.config,
            render::ShellRenderState {
                machine_diagnostics: &self.machine_diagnostics,
                endpoints: &self.endpoints,
                active_endpoint_id: &self.active_endpoint_id,
                collapsed_endpoints: &self.collapsed_endpoints,
                workspace_scroll: &mut self.workspace_scroll,
                agent_scroll: &mut self.agent_scroll,
                tab_scroll: &mut self.tab_scroll,
                reveal_focused_workspace: &mut self.reveal_focused_workspace,
                reveal_focused_tab: &mut self.reveal_focused_tab,
                sidebar_collapsed: self.sidebar_collapsed,
                sidebar_section_split: self.sidebar_section_split,
                tab_drag_insert_index,
                selected_workspace_id: self
                    .navigate_workspace_id
                    .as_ref()
                    .filter(|_| valid_navigation_target)
                    .or_else(|| pending_workspace_highlight.map(|pending| &pending.target)),
                reveal_navigation_workspace: &mut self.reveal_navigation_workspace,
                dragged_workspace_id,
                workspace_drop_indicator_row,
            },
        );
        // The surface may have been produced for another layout: a resize or sidebar toggle
        // keeps the retained surface until the resized one arrives, a resize can race a surface
        // already in flight, and the tab bar appears (shrinking the pane area by a row) when a
        // second tab opens. `blit_pane_surface` clips the cells; the hits are clipped to match
        // (`clip_pane_hit`), so mouse input and the copy cursor never target rows or
        // columns that are not on screen. Later draws that use these rects still go through
        // `Buffer::cell_mut`, never `buffer[(x, y)]`.
        let surface_overflows = surface_overflows_area(surface, layout.pane_surface);
        self.hits.panes = surface
            .panes
            .iter()
            .filter_map(|pane| {
                let hit = PaneHit {
                    rect: Rect::new(
                        layout.pane_surface.x.saturating_add(pane.rect.x),
                        layout.pane_surface.y.saturating_add(pane.rect.y),
                        pane.rect.width,
                        pane.rect.height,
                    ),
                    inner_rect: Rect::new(
                        layout.pane_surface.x.saturating_add(pane.inner_rect.x),
                        layout.pane_surface.y.saturating_add(pane.inner_rect.y),
                        pane.inner_rect.width,
                        pane.inner_rect.height,
                    ),
                    scrollbar_rect: pane.scrollbar_rect.map(|rect| {
                        Rect::new(
                            layout.pane_surface.x.saturating_add(rect.x),
                            layout.pane_surface.y.saturating_add(rect.y),
                            rect.width,
                            rect.height,
                        )
                    }),
                    scroll: pane.scroll.map(|metrics| crate::pane::ScrollMetrics {
                        offset_from_bottom: usize::try_from(metrics.offset_from_bottom)
                            .unwrap_or(usize::MAX),
                        max_offset_from_bottom: usize::try_from(metrics.max_offset_from_bottom)
                            .unwrap_or(usize::MAX),
                        viewport_rows: usize::try_from(metrics.viewport_rows).unwrap_or(usize::MAX),
                        history_origin: metrics.history_origin,
                    }),
                    pane_id: pane.pane_id.clone(),
                    mouse_reporting: pane.mouse_reporting,
                    sgr_pixel_mouse: pane.sgr_pixel_mouse,
                    pixel_width: pane.pixel_width,
                    pixel_height: pane.pixel_height,
                };
                if surface_overflows {
                    clip_pane_hit(hit, layout.pane_surface)
                } else {
                    Some(hit)
                }
            })
            .collect();
        let topology_signature = pane_surface_topology_signature(surface);
        self.hits.pane_splits = surface
            .splits
            .iter()
            // A split dragged against geometry the screen does not show would send ratios
            // computed from the wrong extent; splits wait for a surface that fits.
            .filter(|_| !surface_overflows)
            .map(|split| PaneSplitHit {
                direction: split.direction,
                pos: match split.direction {
                    crate::protocol::PaneSurfaceSplitDirection::Horizontal => {
                        layout.pane_surface.x.saturating_add(split.pos)
                    }
                    crate::protocol::PaneSurfaceSplitDirection::Vertical => {
                        layout.pane_surface.y.saturating_add(split.pos)
                    }
                },
                area: Rect::new(
                    layout.pane_surface.x.saturating_add(split.area.x),
                    layout.pane_surface.y.saturating_add(split.area.y),
                    split.area.width,
                    split.area.height,
                ),
                hit_rect: Rect::new(
                    layout.pane_surface.x.saturating_add(split.hit_rect.x),
                    layout.pane_surface.y.saturating_add(split.hit_rect.y),
                    split.hit_rect.width,
                    split.hit_rect.height,
                ),
                path: split.path.clone(),
                topology_signature,
            })
            .collect();
        if !self.config.mouse_capture {
            self.hits.pane_splits.clear();
        }
        let mode_bar_area = if self.config.tab_bar_position == TabBarPositionConfig::Bottom
            && !layout.tab_bar.is_empty()
        {
            layout.tab_bar
        } else {
            // The bar normally covers the pane area's bottom row. When the copy cursor sits on
            // that row (the last line of history, which scrolling cannot lift, or a pane too
            // short to reserve it) the bar moves to the top row so the cursor stays visible.
            let bottom_row = layout.pane_surface.bottom().saturating_sub(1);
            let copy_cursor_row = (self.mode == ClientShellMode::Copy)
                .then(|| {
                    self.copy_mode
                        .as_ref()
                        .and_then(|copy_mode| client_copy_cursor_cell(copy_mode, &self.hits.panes))
                })
                .flatten()
                .map(|(_, y)| y);
            if layout.pane_surface.height > 1 && copy_cursor_row == Some(bottom_row) {
                Rect::new(
                    layout.pane_surface.x,
                    layout.pane_surface.y,
                    layout.pane_surface.width,
                    1,
                )
            } else {
                layout.pane_surface
            }
        };
        let mode_bar = if self.overlay.is_some() {
            None
        } else {
            render::render_mode_bar(
                &mut buffer,
                mode_bar_area,
                self.mode,
                self.copy_mode.as_ref(),
                self.endpoint_error.as_deref(),
                &self.config.keybinds,
                &self.config.palette,
            )
        };
        if mode_bar == Some(layout.tab_bar) {
            self.hits.tabs.clear();
            self.hits.new_tab = Rect::default();
            self.hits.tab_scroll_left = Rect::default();
            self.hits.tab_scroll_right = Rect::default();
        }
        let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        let mode_bar_cells = mode_bar
            .and_then(|bar| mode_bar_range(&frame, bar))
            .map(|range| frame.cells[range].to_vec());
        blit_pane_surface(&mut frame, &surface.frame, layout.pane_surface);
        restore_mode_bar(&mut frame, mode_bar, mode_bar_cells.as_deref());
        let has_selection = self
            .selection
            .as_ref()
            .is_some_and(crate::vt::selection::Selection::is_visible);
        let has_search = self
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| !copy_mode.search_matches.is_empty());
        if has_selection || has_search {
            let cursor = frame.cursor.clone();
            let mut composed = frame.to_ratatui_buffer()?;
            for hit in &self.hits.panes {
                let copy_surface_coherent =
                    client_copy_surface_coherent(self.copy_mode.as_ref(), hit);
                if copy_surface_coherent {
                    render_client_copy_search_highlights(
                        &mut composed,
                        self.copy_mode.as_ref(),
                        hit,
                        &self.config.palette,
                        false,
                    );
                }
                let selection_is_stale_copy_projection = !copy_surface_coherent
                    && self.copy_mode.as_ref().is_some_and(|copy_mode| {
                        copy_mode.pane_id == hit.pane_id
                            && self
                                .selection
                                .as_ref()
                                .is_some_and(|selection| selection.pane_id == hit.pane_id)
                    });
                if !selection_is_stale_copy_projection {
                    crate::ui::render_selection_highlight(
                        self.selection.as_ref(),
                        &mut composed,
                        &hit.pane_id,
                        hit.inner_rect,
                        hit.scroll,
                        &self.config.palette,
                        crate::host_term::theme::TerminalTheme {
                            background: self.host_background,
                            ..Default::default()
                        },
                    );
                }
                if copy_surface_coherent {
                    render_client_copy_search_highlights(
                        &mut composed,
                        self.copy_mode.as_ref(),
                        hit,
                        &self.config.palette,
                        true,
                    );
                }
            }
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        if self.mode == ClientShellMode::Copy {
            frame.cursor = None;
            if let Some((x, y)) = self
                .copy_mode
                .as_ref()
                .and_then(|copy_mode| client_copy_cursor_cell(copy_mode, &self.hits.panes))
                && x < frame.width
                && y < frame.height
            {
                let mut composed = frame.to_ratatui_buffer()?;
                if let Some(cell) = composed.cell_mut((x, y)) {
                    cell.set_style(
                        Style::default()
                            .fg(match self.config.palette.panel_bg {
                                ratatui::style::Color::Reset => self.config.palette.surface_dim,
                                color => color,
                            })
                            .bg(self.config.palette.accent)
                            .add_modifier(Modifier::BOLD),
                    );
                }
                frame.replace_from_ratatui_buffer_preserving_effects(&composed, None);
            }
        }
        restore_mode_bar(&mut frame, mode_bar, mode_bar_cells.as_deref());
        self.hits.notification_toast = Rect::default();
        let active_lifecycle = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .filter(|endpoint| endpoint.status != ClientEndpointStatus::Online)
            .map(|endpoint| (endpoint.label.clone(), endpoint.status));
        if active_lifecycle.is_some() || self.visible_endpoint_notice.is_some() {
            let cursor = frame.cursor.clone();
            let mut composed = frame.to_ratatui_buffer()?;
            let lifecycle_offset = active_lifecycle.as_ref().map_or(0, |(label, status)| {
                endpoint_notices::render_lifecycle_banner(
                    &mut composed,
                    Rect::new(0, 0, cols, rows),
                    label,
                    *status,
                    &self.config.palette,
                );
                1
            });
            if let Some(notice) = self.visible_endpoint_notice.as_ref() {
                self.hits.notification_toast = endpoint_notices::render_notice(
                    &mut composed,
                    Rect::new(0, 0, cols, rows),
                    notice,
                    lifecycle_offset,
                    &self.config.palette,
                );
            }
            frame.replace_from_ratatui_buffer_preserving_effects(&composed, cursor);
        }
        restore_mode_bar(&mut frame, mode_bar, mode_bar_cells.as_deref());
        if let Some(overlay) = self.overlay.as_ref() {
            let mut composed = frame.to_ratatui_buffer()?;
            let rendered = match overlay {
                ClientShellOverlay::ContextMenu(menu) => {
                    render::render_context_menu(&mut composed, menu, &self.config.palette)
                }
                ClientShellOverlay::GlobalMenu(menu) => render::render_global_menu(
                    &mut composed,
                    self.hits.global_launcher,
                    menu,
                    snapshot,
                    &self.config.palette,
                ),
                _ => render::render_client_overlay(
                    &mut composed,
                    overlay,
                    snapshot,
                    &self.endpoints,
                    &self.active_endpoint_id,
                    &self.config.keybinds,
                    &self.config.palette,
                ),
            };
            if let Some(rendered) = rendered {
                match overlay {
                    ClientShellOverlay::ContextMenu(_) => {
                        self.hits.context_menu_rows = rendered.menu_rows;
                    }
                    ClientShellOverlay::GlobalMenu(_) => {
                        self.hits.global_menu_rows = rendered.menu_rows;
                    }
                    _ => {}
                }
                self.hits.overlay_primary = rendered.primary;
                self.hits.overlay_clear = rendered.clear;
                self.hits.overlay_cancel = rendered.cancel;
                self.hits.navigator_popup = rendered.navigator_popup;
                self.hits.navigator_search = rendered.navigator_search;
                self.hits.navigator_rows = rendered.navigator_rows;
                self.hits.navigator_scrollbar = rendered.navigator_scrollbar;
                self.hits.navigator_scroll_metrics = rendered.navigator_scroll_metrics;
                self.hits.help_popup = rendered.help_popup;
                self.hits.help_scrollbar = rendered.help_scrollbar;
                self.hits.help_scroll_metrics = rendered.help_scroll_metrics;
                self.hits.help_max_scroll = rendered.help_max_scroll;
                frame.replace_from_ratatui_buffer_preserving_effects(&composed, rendered.cursor);
            } else {
                // The overlay does not fit this terminal. Its renderer may have drawn part of
                // itself into `composed` before giving up, so that buffer is dropped and the
                // frame without the overlay is presented: pane output keeps flowing and a
                // one-line hint says why the overlay is missing. The overlay stays open (its
                // keys still work, esc closes it) and reappears once the terminal is large
                // enough. Overlay hit rects stay empty, so mouse input cannot hit an
                // invisible popup.
                let mut hint = frame.to_ratatui_buffer()?;
                let hint_row = rows.saturating_sub(1);
                let hint_style = Style::default()
                    .fg(panel_contrast_fg(&self.config.palette))
                    .bg(self.config.palette.accent)
                    .add_modifier(Modifier::BOLD);
                hint.set_style(Rect::new(0, hint_row, cols, 1.min(rows)), hint_style);
                render::put_text(
                    &mut hint,
                    0,
                    hint_row,
                    cols,
                    " window too small for this popup · esc closes",
                    hint_style,
                );
                frame.replace_from_ratatui_buffer_preserving_effects(&hint, None);
            }
        }
        if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
            help.scroll = help.scroll.min(self.hits.help_max_scroll);
        }
        if self.endpoint_status(&self.active_endpoint_id) != Some(ClientEndpointStatus::Online) {
            frame.cursor = None;
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
        }
        // This path draws a visible notice unconditionally (above), so this is where its
        // lifetime starts.
        self.endpoint_notice_drawn(std::time::Instant::now());
        Some(crate::client::frame_output::ComposedFrame { frame })
    }
}

/// Whether the retained surface reaches past the pane area it is drawn into. A smaller surface
/// (the panes have not grown into a larger area yet) is drawn whole and its hits stay exact.
pub(super) fn surface_overflows_area(surface: &PaneSurfaceFrame, area: Rect) -> bool {
    surface.frame.width > area.width || surface.frame.height > area.height
}

fn clip_rect(rect: Rect, area: Rect) -> Rect {
    let x = rect.x.max(area.x);
    let y = rect.y.max(area.y);
    let right = rect.right().min(area.right());
    let bottom = rect.bottom().min(area.bottom());
    Rect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

/// Clips a pane hit from an oversized surface to the visible pane area. Surface rects start at
/// or after the area's origin, so clipping keeps each origin and mouse coordinates keep mapping
/// to the same pane cells; a pane with no visible content cell is dropped. Pixel extents
/// describe the whole pane and would stretch over the clipped rect, so a clipped hit reports
/// cell positions only. Copy-mode coherence compares geometry with `inner_rect`, so the copy
/// cursor and search highlights of a clipped pane wait for a surface that fits.
fn clip_pane_hit(mut hit: PaneHit, area: Rect) -> Option<PaneHit> {
    let inner = clip_rect(hit.inner_rect, area);
    if inner.is_empty() {
        return None;
    }
    if inner != hit.inner_rect {
        hit.pixel_width = 0;
        hit.pixel_height = 0;
    }
    hit.inner_rect = inner;
    hit.rect = clip_rect(hit.rect, area);
    hit.scrollbar_rect = hit
        .scrollbar_rect
        .map(|rect| clip_rect(rect, area))
        .filter(|rect| !rect.is_empty());
    Some(hit)
}

fn client_copy_surface_coherent(copy_mode: Option<&ClientCopyModeState>, hit: &PaneHit) -> bool {
    copy_mode
        .filter(|copy_mode| copy_mode.pane_id == hit.pane_id)
        .is_none_or(|copy_mode| {
            copy_mode.geometry == (hit.inner_rect.width, hit.inner_rect.height)
                && hit.scroll.is_some_and(|scroll| {
                    scroll.offset_from_bottom == copy_mode.offset_from_bottom
                        && scroll.max_offset_from_bottom == copy_mode.max_offset_from_bottom
                })
        })
}

/// Screen cell of the copy-mode cursor, when its pane is on screen, coherent with the copy
/// state, and the cursor row is inside the pane's viewport.
fn client_copy_cursor_cell(
    copy_mode: &ClientCopyModeState,
    hits: &[PaneHit],
) -> Option<(u16, u16)> {
    let hit = hits.iter().find(|hit| {
        hit.pane_id == copy_mode.pane_id && client_copy_surface_coherent(Some(copy_mode), hit)
    })?;
    let viewport_top = crate::vt::ScreenRow(
        copy_mode
            .max_offset_from_bottom
            .saturating_sub(copy_mode.offset_from_bottom),
    );
    let viewport_row = copy_mode.cursor.row.0.checked_sub(viewport_top.0)?;
    if viewport_row >= usize::from(hit.inner_rect.height)
        || copy_mode.cursor.col >= hit.inner_rect.width
    {
        return None;
    }
    Some((
        hit.inner_rect.x.saturating_add(copy_mode.cursor.col),
        hit.inner_rect
            .y
            .saturating_add(u16::try_from(viewport_row).unwrap_or(u16::MAX)),
    ))
}

fn render_client_copy_search_highlights(
    buffer: &mut Buffer,
    copy_mode: Option<&ClientCopyModeState>,
    hit: &PaneHit,
    palette: &Palette,
    current_only: bool,
) {
    let Some(copy_mode) = copy_mode.filter(|copy_mode| copy_mode.pane_id == hit.pane_id) else {
        return;
    };
    if hit.inner_rect.is_empty() {
        return;
    }
    let top = crate::vt::ScreenRow(
        copy_mode
            .max_offset_from_bottom
            .saturating_sub(copy_mode.offset_from_bottom),
    );
    let bottom = crate::vt::ScreenRow(
        top.0
            .saturating_add(usize::from(hit.inner_rect.height.saturating_sub(1))),
    );
    let style = if current_only {
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.surface1)
    };
    for (index, text_match) in copy_mode.search_matches.iter().enumerate() {
        if (copy_mode.search_current == Some(index)) != current_only
            || text_match.end.row < top
            || text_match.start.row > bottom
        {
            continue;
        }
        let start_row = text_match.start.row.max(top);
        let end_row = text_match.end.row.min(bottom);
        for absolute_row in start_row.0..=end_row.0 {
            let viewport_row =
                u16::try_from(absolute_row.saturating_sub(top.0)).unwrap_or(u16::MAX);
            let start_col = if absolute_row == text_match.start.row.0 {
                text_match.start.col
            } else {
                0
            };
            let end_col = if absolute_row == text_match.end.row.0 {
                text_match.end.col
            } else {
                hit.inner_rect.width.saturating_sub(1)
            };
            let end_col = end_col.min(hit.inner_rect.width.saturating_sub(1));
            // The hit comes from the pane surface, whose geometry may have been produced for
            // a different layout than this frame (see where `compose` builds pane hits).
            // `cell_mut` skips cells outside the buffer where `Buffer` indexing would panic.
            for col in start_col..=end_col {
                let (Some(x), Some(y)) = (
                    hit.inner_rect.x.checked_add(col),
                    hit.inner_rect.y.checked_add(viewport_row),
                ) else {
                    continue;
                };
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_style(style);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{PaneTextPoint, PaneTextRange};

    fn text_range(row: usize, start_col: u16, end_col: u16) -> PaneTextRange {
        PaneTextRange {
            start: PaneTextPoint {
                row: crate::vt::ScreenRow(row),
                col: start_col,
            },
            end: PaneTextPoint {
                row: crate::vt::ScreenRow(row),
                col: end_col,
            },
        }
    }

    #[test]
    fn copy_search_highlights_clip_surface_taller_than_frame() {
        // A pane surface produced for another layout (e.g. before the tab bar appeared)
        // is one row taller than the frame. Matches on its bottom row are off-buffer
        // and must be skipped instead of panicking on `Buffer` indexing.
        let hit = PaneHit {
            rect: Rect::new(0, 0, 6, 4),
            inner_rect: Rect::new(0, 0, 6, 4),
            scrollbar_rect: None,
            scroll: Some(crate::pane::ScrollMetrics {
                offset_from_bottom: 0,
                max_offset_from_bottom: 0,
                viewport_rows: 4,
                history_origin: crate::vt::AbsRow(0),
            }),
            pane_id: "pane".into(),
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        let copy_mode = ClientCopyModeState {
            pane_id: "pane".into(),
            content_revision: 0,
            geometry: (6, 4),
            alternate_screen_active: false,
            cursor: PaneTextPoint {
                row: crate::vt::ScreenRow(3),
                col: 0,
            },
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            entry_offset_from_bottom: 0,
            selection: None,
            search_prompt: None,
            search_query: "x".to_string(),
            search_direction: None,
            search_matches: vec![text_range(2, 0, 1), text_range(3, 0, 5)],
            search_total: 2,
            search_current: Some(1),
            search_current_global: Some(1),
            search_generation: 0,
            copy_after_search: false,
        };
        let palette = Palette::catppuccin();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 3));

        for current_only in [false, true] {
            render_client_copy_search_highlights(
                &mut buffer,
                Some(&copy_mode),
                &hit,
                &palette,
                current_only,
            );
        }

        assert_eq!(buffer[(0, 2)].style().bg, Some(palette.surface1));
        assert_eq!(buffer[(1, 2)].style().bg, Some(palette.surface1));
        assert_ne!(buffer[(2, 2)].style().bg, Some(palette.surface1));
    }
}
