use super::*;

#[path = "wire_cells.rs"]
pub(in crate::shell) mod wire_cells;
use wire_cells::{StylePatch, overwrite, patch_cell, patch_rect, patch_style};

impl ClientShellState {
    pub(crate) fn compose(&mut self, cols: u16, rows: u16) -> Option<FrameData> {
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
        // Only the exact snapshot pair is drawn. With nothing presented the placeholder
        // layer is drawn; while a presented surface is held unpaired (the snapshot passed
        // it, a baseline waits for its snapshot, or the connection was lost) the last
        // frame stays on screen until the matching pair exists.
        let has_surface = self.snapshot.is_some() && self.pane_surface().is_some();
        if has_surface && !self.surfaces.is_paired() {
            return None;
        }
        let layout = self.layout(cols, rows);
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
        if !has_surface {
            buffer.set_style(
                buffer.area,
                Style::default()
                    .fg(self.config.palette.text)
                    .bg(self.config.palette.panel_bg),
            );
        }
        let mut chrome_layout = layout;
        if !has_surface && layout.sidebar.width == 0 {
            chrome_layout.sidebar = Rect::new(0, 1, cols, rows.saturating_sub(2));
        }

        self.hits = render::render_shell(
            &mut buffer,
            chrome_layout,
            self.snapshot.as_deref(),
            &self.config,
            render::ShellRenderState {
                machine_diagnostics: &self.machine_diagnostics,
                endpoints: &self.endpoints,
                active_endpoint_id: &self.active_endpoint_id,
                collapsed_endpoints: &self.collapsed_endpoints,
                workspace_scroll: &mut self.workspace_scroll,
                agent_scroll: &mut self.agent_scroll,
                reveal_focused_workspace: &mut self.reveal_focused_workspace,
                sidebar_collapsed: layout.sidebar.width > 0 && self.sidebar_collapsed,
                sidebar_section_split: self.sidebar_section_split,
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
        let active_lifecycle = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .filter(|endpoint| endpoint.status != ClientEndpointStatus::Online)
            .map(|endpoint| {
                (
                    endpoint.endpoint_id.display_label().to_owned(),
                    endpoint.status,
                )
            });
        let healthy_local_chrome = self.snapshot.is_some()
            && self.endpoints.len() == 1
            && !self.sidebar_collapsed
            && layout.sidebar.width > 0
            && self.endpoint_status(&self.active_endpoint_id) == Some(ClientEndpointStatus::Online);
        if !has_surface {
            let message = self.endpoint_error.clone().unwrap_or_else(|| {
                let status = self
                    .endpoint_status(&self.active_endpoint_id)
                    .unwrap_or(ClientEndpointStatus::Connecting);
                let (_, label, _) = endpoint_status_presentation(status, &self.config.palette);
                if self.endpoints.len() == 1 {
                    format!("{}: {label}.", self.active_endpoint_label())
                } else {
                    format!(
                        "{}: {label}. Select a connected machine.",
                        self.active_endpoint_label()
                    )
                }
            });
            let message_area = if layout.sidebar.width > 0 {
                layout.pane_surface
            } else {
                Rect::new(0, 0, cols, 1)
            };
            // The lifecycle banner already carries the placeholder status. A second status
            // line in the same row can be covered by that banner on narrow surfaces. An
            // endpoint error suppressed here still shows in the mode bar.
            if (!healthy_local_chrome || self.endpoint_error.is_some())
                && active_lifecycle.is_none()
            {
                render::put_text(
                    &mut buffer,
                    message_area.x,
                    message_area.y,
                    message_area.width,
                    &message,
                    Style::default().fg(self.config.palette.overlay0),
                );
            }
        }
        // Pane cells and their local decorations are one optional layer. Everything above
        // them (lifecycle, notices, overlays and the mode bar) follows the same pipeline
        // when the pane area contains only a connection placeholder.
        let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]);
        if let Some(surface) = self.surfaces.paired().filter(|_| has_surface) {
            // The surface may have been produced for another layout: a resize or sidebar toggle
            // keeps the retained surface until the resized one arrives, a resize can race a surface
            // already in flight. `compose_pane_surface` clips the cells; the hits are clipped to
            // match (`clip_pane_hit`), so mouse input and the copy cursor never target rows or
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
                        scroll: pane.scroll.map(|metrics| shepr_termio::ScrollMetrics {
                            offset_from_bottom: usize::try_from(metrics.offset_from_bottom)
                                .unwrap_or(usize::MAX),
                            max_offset_from_bottom: usize::try_from(metrics.max_offset_from_bottom)
                                .unwrap_or(usize::MAX),
                            viewport_rows: usize::try_from(metrics.viewport_rows)
                                .unwrap_or(usize::MAX),
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
                        shepr_protocol::PaneSurfaceSplitDirection::Horizontal => {
                            layout.pane_surface.x.saturating_add(split.pos)
                        }
                        shepr_protocol::PaneSurfaceSplitDirection::Vertical => {
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
            // Chrome is the only thing drawn through ratatui here; from this point the frame's
            // wire cells are the composition target and every later stage patches or overwrites
            // them in place (see `wire_cells`).
            compose_pane_surface(&mut frame, &surface.frame, layout.pane_surface);
            let has_selection = self
                .selection
                .as_ref()
                .is_some_and(shepr_vt::selection::Selection::is_visible);
            let has_search = self
                .copy_mode
                .as_ref()
                .is_some_and(|copy_mode| !copy_mode.search_matches.is_empty());
            // Highlights restyle wire cells in the existing order: noncurrent search matches,
            // selection, the current search match, then the copy cursor.
            if has_selection || has_search {
                for hit in &self.hits.panes {
                    let copy_surface_coherent =
                        client_copy_surface_coherent(self.copy_mode.as_ref(), hit);
                    if copy_surface_coherent {
                        render_client_copy_search_highlights(
                            &mut frame,
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
                        shepr_termio::selection_render::render_selection_highlight(
                            self.selection.as_ref(),
                            &hit.pane_id,
                            hit.inner_rect,
                            hit.scroll,
                            &self.config.palette,
                            shepr_termio::host_term::theme::TerminalTheme {
                                background: self.host_background,
                                ..Default::default()
                            },
                            &mut |x, y, style| {
                                patch_cell(&mut frame, x, y, StylePatch::from_style(style));
                            },
                        );
                    }
                    if copy_surface_coherent {
                        render_client_copy_search_highlights(
                            &mut frame,
                            self.copy_mode.as_ref(),
                            hit,
                            &self.config.palette,
                            true,
                        );
                    }
                }
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
                    patch_cell(
                        &mut frame,
                        x,
                        y,
                        StylePatch::from_style(
                            Style::default()
                                .fg(match self.config.palette.panel_bg {
                                    ratatui::style::Color::Reset => self.config.palette.surface_dim,
                                    color => color,
                                })
                                .bg(self.config.palette.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                    );
                }
            }
        }
        // The bar normally covers the pane area's bottom row. When the copy cursor sits on
        // that row (the last line of history, which scrolling cannot lift, or a pane too
        // short to reserve it) the bar moves to the top row so the cursor stays visible.
        let mode_bar_area = if !has_surface {
            Rect::new(0, 0, cols, rows)
        } else {
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
        if self.endpoint_status(&self.active_endpoint_id) != Some(ClientEndpointStatus::Online) {
            frame.cursor = None;
            self.hits.panes.clear();
            self.hits.pane_splits.clear();
        }
        self.hits.notification_toast = Rect::default();
        if active_lifecycle.is_some() || self.visible_endpoint_notice.is_some() {
            // Banner and card are opaque: they draw into a fresh scratch buffer and return
            // their `Clear` rects, and the frame takes exactly those rects.
            let mut scratch = Buffer::empty(Rect::new(0, 0, cols, rows));
            let mut opaque = Vec::new();
            let lifecycle_offset = active_lifecycle.as_ref().map_or(0, |(label, status)| {
                let area = if !has_surface && layout.sidebar.width > 0 {
                    layout.pane_surface
                } else {
                    Rect::new(0, 0, cols, rows)
                };
                opaque.push(endpoint_notices::render_lifecycle_banner(
                    &mut scratch,
                    area,
                    label,
                    *status,
                    &self.config.palette,
                ));
                1
            });
            let notice_offset = if has_surface {
                lifecycle_offset
            } else if layout.sidebar.width == 0 {
                // The fallback sidebar starts below the placeholder line when its normal
                // column is hidden, so leave its header row clear as well.
                2
            } else {
                // An expanded sidebar has its header on row zero, even without a pane surface.
                1
            };
            if let Some(notice) = self.visible_endpoint_notice.as_ref() {
                self.hits.notification_toast = endpoint_notices::render_notice(
                    &mut scratch,
                    Rect::new(0, 0, cols, rows),
                    notice,
                    notice_offset,
                    &self.config.palette,
                );
                opaque.push(self.hits.notification_toast);
            }
            overwrite(&mut frame, &opaque, &scratch);
        }
        if let Some(overlay) = self.overlay.as_ref() {
            // Every overlay renderer draws into a fresh scratch buffer and reports what it
            // painted. The frame is touched only when the renderer succeeds, so one that
            // gives up leaves it exactly as it was for the fallback hint below.
            let mut scratch = Buffer::empty(Rect::new(0, 0, cols, rows));
            let rendered = match overlay {
                ClientShellOverlay::ContextMenu(menu) => {
                    render::render_context_menu(&mut scratch, menu, &self.config.palette)
                }
                ClientShellOverlay::GlobalMenu(menu) => {
                    render::render_global_menu(&mut scratch, menu, &self.config.palette)
                }
                _ => render::render_client_overlay(
                    &mut scratch,
                    overlay,
                    &self.endpoints,
                    &self.active_endpoint_id,
                    &self.config.keybinds,
                    &self.config.palette,
                ),
            };
            if let Some(rendered) = rendered {
                if rendered.backdrop {
                    patch_style(
                        &mut frame.cells,
                        StylePatch::from_style(Style::default().add_modifier(Modifier::DIM)),
                    );
                }
                overwrite(&mut frame, &rendered.opaque, &scratch);
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
                frame.cursor = rendered.cursor;
            } else {
                // The overlay does not fit this terminal, and nothing it drew was committed:
                // the frame without the overlay is presented, so pane output keeps flowing,
                // and a one-line hint says why the overlay is missing. The overlay stays
                // open (its keys still work, esc closes it) and reappears once the terminal
                // is large enough. Overlay hit rects stay empty, so mouse input cannot hit
                // an invisible popup.
                if cols > 0 && rows > 0 {
                    let hint_row = rows - 1;
                    let hint_style = Style::default()
                        .fg(panel_contrast_fg(&self.config.palette))
                        .bg(self.config.palette.accent)
                        .add_modifier(Modifier::BOLD);
                    // The whole row takes the hint's style; the text replaces the prefix
                    // the shared text writer placed, whose extent is the overwritten rect
                    // (a wide glyph the text boundary splits is blanked).
                    patch_rect(
                        &mut frame,
                        Rect::new(0, hint_row, cols, 1),
                        StylePatch::from_style(hint_style),
                    );
                    let mut scratch = Buffer::empty(Rect::new(0, 0, cols, rows));
                    let written_to = super::render::put_text(
                        &mut scratch,
                        0,
                        hint_row,
                        cols,
                        " window too small for this popup · esc closes",
                        hint_style,
                    );
                    overwrite(
                        &mut frame,
                        &[Rect::new(0, hint_row, written_to, 1)],
                        &scratch,
                    );
                }
                frame.cursor = None;
            }
        }
        // The mode bar is drawn last, after banners and notices, and only when no overlay is
        // open (an overlay that does not fit still counts as open). It draws into a scratch
        // buffer like the overlays. Its pane-cursor suppression is its own rule, distinct
        // from an overlay's returned input cursor: a pane cursor on the bar's row would
        // show through the bar.
        if self.overlay.is_none() {
            let mut scratch = Buffer::empty(Rect::new(0, 0, cols, rows));
            if let Some(bar) = render::render_mode_bar(
                &mut scratch,
                mode_bar_area,
                self.mode,
                self.copy_mode.as_ref(),
                self.endpoint_error.as_deref(),
                &self.config.keybinds,
                &self.config.palette,
            ) {
                overwrite(&mut frame, &[bar], &scratch);
                if frame
                    .cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.y == bar.y)
                {
                    frame.cursor = None;
                }
            }
        }
        if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
            help.scroll = help.scroll.min(self.hits.help_max_scroll);
        }
        // Both pane layers pass through the notice stage, so its lifetime starts here.
        self.endpoint_notice_drawn(self.now);
        self.record_composed_frame();
        Some(frame)
    }

    fn record_composed_frame(&mut self) {
        self.last_composed_at = Some(self.now);
        self.selection_repaint_deadline = None;
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
                        && scroll.history_origin == copy_mode.history_origin
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
    let viewport_row = copy_mode
        .cursor
        .row
        .0
        .checked_sub(copy_mode.viewport_top().0)?;
    let viewport_row = u16::try_from(viewport_row)
        .ok()
        .filter(|row| *row < hit.inner_rect.height)?;
    if copy_mode.cursor.col >= hit.inner_rect.width {
        return None;
    }
    Some((
        hit.inner_rect.x.saturating_add(copy_mode.cursor.col),
        hit.inner_rect.y.saturating_add(viewport_row),
    ))
}

fn render_client_copy_search_highlights(
    frame: &mut FrameData,
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
    let top = copy_mode.viewport_top();
    let bottom = top.saturating_add(u64::from(hit.inner_rect.height.saturating_sub(1)));
    let patch = StylePatch::from_style(if current_only {
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text).bg(palette.surface1)
    });
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
            // `patch_cell` skips positions outside the frame.
            for col in start_col..=end_col {
                let (Some(x), Some(y)) = (
                    hit.inner_rect.x.checked_add(col),
                    hit.inner_rect.y.checked_add(viewport_row),
                ) else {
                    continue;
                };
                patch_cell(frame, x, y, patch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::command::{PaneTextPoint, PaneTextRange};

    fn frame_row_text(frame: &FrameData, y: u16) -> String {
        let start = usize::from(y) * usize::from(frame.width);
        frame.cells[start..start + usize::from(frame.width)]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    fn text_range(row: u64, start_col: u16, end_col: u16) -> PaneTextRange {
        PaneTextRange {
            start: PaneTextPoint {
                row: shepr_vt::AbsRow(row),
                col: start_col,
            },
            end: PaneTextPoint {
                row: shepr_vt::AbsRow(row),
                col: end_col,
            },
        }
    }

    #[test]
    fn copy_search_highlights_clip_surface_taller_than_frame() {
        // A pane surface produced for another layout (e.g. before a resize took effect)
        // is one row taller than the frame. Matches on its bottom row are off-buffer
        // and must be skipped instead of panicking on `Buffer` indexing.
        let hit = PaneHit {
            rect: Rect::new(0, 0, 6, 4),
            inner_rect: Rect::new(0, 0, 6, 4),
            scrollbar_rect: None,
            scroll: Some(shepr_termio::ScrollMetrics {
                offset_from_bottom: 0,
                max_offset_from_bottom: 0,
                viewport_rows: 4,
                history_origin: shepr_vt::AbsRow(0),
            }),
            pane_id: crate::tests::test_pane_id("w1:p1"),
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        let copy_mode = ClientCopyModeState {
            pane_id: crate::tests::test_pane_id("w1:p1"),
            geometry: (6, 4),
            alternate_screen_active: false,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(3),
                col: 0,
            },
            history_origin: shepr_vt::AbsRow(0),
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
        let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(
            &Buffer::empty(Rect::new(0, 0, 6, 3)),
            None,
            &[],
        );

        for current_only in [false, true] {
            render_client_copy_search_highlights(
                &mut frame,
                Some(&copy_mode),
                &hit,
                &palette,
                current_only,
            );
        }

        let bg = |x: usize, y: usize| frame.cells[y * 6 + x].bg;
        let surface1 = shepr_protocol::WireColor::from_ratatui(palette.surface1);
        assert_eq!(bg(0, 2), surface1);
        assert_eq!(bg(1, 2), surface1);
        assert_ne!(bg(2, 2), surface1);
    }

    #[test]
    fn placeholder_lifecycle_and_notice_leave_the_hidden_sidebar_header_clear() {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.config.sidebar_collapsed_mode = SidebarCollapsedModeConfig::Hidden;
        state.sidebar_collapsed = true;
        state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Reconnecting);
        assert!(state.push_endpoint_notice(
            ClientEndpointNoticeKind::Unavailable,
            "unavailable",
            "Server unavailable",
            "Waiting for the server",
        ));

        let frame = state.compose(34, 12).expect("placeholder frame");

        let lifecycle_row = frame_row_text(&frame, 0);
        assert!(lifecycle_row.contains("reconnecting"));
        assert!(!lifecycle_row.contains("Local:"));
        assert!(frame_row_text(&frame, 1).contains("spaces"));
        assert!(state.hits.notification_toast.y >= 2);
    }

    #[test]
    fn online_placeholder_notice_starts_below_the_expanded_sidebar_header() {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.sidebar_collapsed = false;
        state.sidebar_width = 24;
        state.set_snapshot(Box::new(crate::shell::tests::snapshot()));
        assert!(state.push_endpoint_notice(
            ClientEndpointNoticeKind::Unavailable,
            "unavailable",
            "Server unavailable",
            "Waiting for the server",
        ));

        let frame = state.compose(80, 12).expect("placeholder frame");

        assert!(frame_row_text(&frame, 0).contains("spaces"));
        assert!(state.hits.notification_toast.y >= 1);
    }
}
