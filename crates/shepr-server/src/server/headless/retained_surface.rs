use super::*;
use crate::server::ClientId;
use crate::server::clients::ClientPaneIdentity;
use tracing::trace;

fn rect_fits_frame(rect: shepr_protocol::SurfaceRect, frame: &FrameData) -> bool {
    rect.x.saturating_add(rect.width) <= frame.width
        && rect.y.saturating_add(rect.height) <= frame.height
}

fn patch_intersects_hyperlinks(
    frame: &FrameData,
    area: shepr_protocol::SurfaceRect,
    patch: &shepr_mux::pane::TerminalDirtyPatch,
) -> bool {
    if frame.hyperlinks.is_empty() || !rect_fits_frame(area, frame) {
        return false;
    }
    let width = usize::from(frame.width);
    patch
        .rows
        .iter()
        .filter(|(local_y, _)| *local_y < area.height)
        .any(|(local_y, _)| {
            let start = usize::from(area.y + *local_y) * width + usize::from(area.x);
            let end = start + usize::from(area.width);
            end > frame.cells.len()
                || frame.cells[start..end]
                    .iter()
                    .any(|cell| cell.hyperlink.is_some())
        })
}

fn patch_row_changed(frame: &FrameData, row: &shepr_protocol::PaneSurfacePatchRow) -> Option<bool> {
    if row.y >= frame.height
        || row.x.saturating_add(u16::try_from(row.cells.len()).ok()?) > frame.width
    {
        return None;
    }
    let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
    let end = start + row.cells.len();
    if end > frame.cells.len() {
        return None;
    }
    Some(frame.cells[start..end] != row.cells)
}

fn changed_rows(
    frame: &FrameData,
    area: shepr_protocol::SurfaceRect,
    patch: &shepr_mux::pane::TerminalDirtyPatch,
) -> Option<Vec<shepr_protocol::PaneSurfacePatchRow>> {
    if !rect_fits_frame(area, frame) {
        return None;
    }
    let mut rows = Vec::new();
    for (local_y, cells) in &patch.rows {
        if *local_y >= area.height {
            continue;
        }
        let width = usize::from(area.width);
        if cells.len() < width {
            return None;
        }
        let y = area.y + *local_y;
        let frame_start = usize::from(y) * usize::from(frame.width) + usize::from(area.x);
        let frame_end = frame_start.checked_add(width)?;
        let existing = frame.cells.get(frame_start..frame_end)?;
        let desired = &cells[..width];
        let mut offset = 0;
        while offset < width {
            if existing[offset] == desired[offset] {
                offset += 1;
                continue;
            }
            let start = offset;
            offset += 1;
            while offset < width && existing[offset] != desired[offset] {
                offset += 1;
            }
            // Include the following cell so a wide-to-narrow (or
            // narrow-to-wide) transition repaints content covered by the old
            // grapheme width even when that logical neighbor is unchanged.
            let end = offset.saturating_add(1).min(width);
            rows.push(shepr_protocol::PaneSurfacePatchRow {
                x: area.x.checked_add(u16::try_from(start).ok()?)?,
                y,
                cells: desired[start..end].to_vec(),
            });
            offset = end;
        }
    }
    Some(rows)
}

fn retained_scrollbar_patch(
    app: &app::App,
    frame: &FrameData,
    pane: &mut shepr_protocol::PaneSurfacePane,
    reserved_gutter: Option<shepr_protocol::SurfaceRect>,
    alternate_screen_active: bool,
    metrics: Option<shepr_mux::pane::ScrollMetrics>,
) -> Option<Vec<shepr_protocol::PaneSurfacePatchRow>> {
    let next_rect = metrics
        .filter(|metrics| metrics.max_offset_from_bottom > 0)
        .filter(|_| app.state.settings.pane_scrollbars && !alternate_screen_active)
        .and(reserved_gutter)
        .filter(|rect| {
            let right = pane.rect.x.saturating_add(pane.rect.width);
            rect_fits_frame(*rect, frame)
                && rect.x >= pane.rect.x
                && rect.x.saturating_add(rect.width) <= right
        });
    let patch_rect = next_rect.or(pane.scrollbar_rect);
    pane.scrollbar_rect = next_rect;
    let Some(rect) = patch_rect else {
        return Some(Vec::new());
    };

    let track = Rect::new(0, 0, 1, rect.height);
    let mut buffer = ratatui::buffer::Buffer::empty(track);
    if let (Some(metrics), Some(_)) = (metrics, next_rect) {
        crate::ui::render_pane_scrollbar_buffer(
            &mut buffer,
            metrics,
            track,
            &app.state.settings.palette,
            pane.focused,
        );
    }
    let cells = buffer
        .content
        .iter()
        .map(shepr_protocol::CellData::from_ratatui_cell)
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    for (offset, cell) in cells.into_iter().enumerate() {
        let row = shepr_protocol::PaneSurfacePatchRow {
            x: rect.x,
            y: rect.y.checked_add(u16::try_from(offset).ok()?)?,
            cells: vec![cell],
        };
        if patch_row_changed(frame, &row)? {
            rows.push(row);
        }
    }
    Some(rows)
}

fn retained_cursor(
    app: &app::App,
    panes: &[ResolvedRetainedPane<'_>],
) -> Option<shepr_protocol::CursorState> {
    let pane = panes.iter().find(|pane| pane.pane.focused)?;
    let runtime = app.state.runtime_for_pane_in_workspace(
        &app.terminal_runtimes,
        pane.workspace_index,
        pane.identity.pane_id,
    )?;
    if runtime.synchronized_output_active() {
        return None;
    }
    let area = Rect::new(
        pane.pane.inner_rect.x,
        pane.pane.inner_rect.y,
        pane.pane.inner_rect.width,
        pane.pane.inner_rect.height,
    );
    runtime
        .cursor_state(area, true)
        .map(|cursor| shepr_protocol::CursorState {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible && !crate::ui::pane_is_scrolled_back(runtime),
            shape: cursor.shape,
        })
}

struct RetainedRecipient<'a> {
    client_id: ClientId,
    surface: &'a shepr_protocol::PaneSurfaceFrame,
    // Reuse the typed identities committed next to this connection's wire pane
    // entries for source matching, synchronized-output checks, and cursor lookup.
    panes: Vec<ResolvedRetainedPane<'a>>,
}

struct ResolvedRetainedPane<'a> {
    pane: &'a shepr_protocol::PaneSurfacePane,
    reserved_scrollbar_gutter: Option<shepr_protocol::SurfaceRect>,
    workspace_index: usize,
    identity: &'a ClientPaneIdentity,
}

struct RetainedPaneLayout {
    workspace_index: usize,
    panes: Vec<shepr_mux::workspace::PaneChromeInfo>,
}

struct CollectedPanePatch {
    identity: ClientPaneIdentity,
    patch: shepr_mux::pane::TerminalDirtyPatch,
    content_revision: u64,
    scroll_metrics: Option<shepr_mux::pane::ScrollMetrics>,
    mouse_reporting: bool,
    sgr_pixel_mouse: bool,
    alternate_screen_active: bool,
}

struct RetainedRecipientUpdate {
    client_id: ClientId,
    patch: shepr_protocol::PaneSurfacePatch,
}

fn resolve_retained_panes<'a>(
    app: &app::App,
    surface: &'a shepr_protocol::PaneSurfaceFrame,
    identities: &'a [ClientPaneIdentity],
    layout: &RetainedPaneLayout,
) -> Option<Vec<ResolvedRetainedPane<'a>>> {
    if surface.panes.len() != identities.len() {
        return None;
    }
    let Some(first_identity) = identities.first() else {
        return Some(Vec::new());
    };
    if identities
        .iter()
        .any(|identity| identity.workspace_id != first_identity.workspace_id)
    {
        return None;
    }
    let workspace = app.state.workspaces.get(layout.workspace_index)?;
    if workspace.id != first_identity.workspace_id || layout.panes.len() != surface.panes.len() {
        return None;
    }
    let mut resolved = Vec::with_capacity(surface.panes.len());
    for ((pane, identity), pane_layout) in surface.panes.iter().zip(identities).zip(&layout.panes) {
        if pane_layout.id != identity.pane_id {
            return None;
        }
        let pane_inner =
            shepr_mux::workspace::pane_inner_rect(pane_layout.rect, pane_layout.borders);
        let content = shepr_mux::workspace::terminal_content_rect(
            pane_inner,
            app.state.settings.pane_scrollbars,
            pane.alternate_screen_active,
        );
        let committed_rect = Rect::new(pane.rect.x, pane.rect.y, pane.rect.width, pane.rect.height);
        let committed_inner = Rect::new(
            pane.inner_rect.x,
            pane.inner_rect.y,
            pane.inner_rect.width,
            pane.inner_rect.height,
        );
        if pane_layout.rect != committed_rect || content != committed_inner {
            return None;
        }

        // The terminal content can end at the pane's right border when the
        // pane is too narrow for a gutter. Derive the track only from the
        // full layout's reserved gutter, never from that content endpoint.
        let reserved_scrollbar_gutter =
            (content != pane_inner).then(|| shepr_protocol::SurfaceRect {
                x: pane_inner
                    .x
                    .saturating_add(pane_inner.width.saturating_sub(1)),
                y: pane_inner.y,
                width: 1,
                height: pane_inner.height,
            });
        if pane.scrollbar_rect.is_some() && pane.scrollbar_rect != reserved_scrollbar_gutter {
            return None;
        }
        resolved.push(ResolvedRetainedPane {
            pane,
            reserved_scrollbar_gutter,
            workspace_index: layout.workspace_index,
            identity,
        });
    }
    Some(resolved)
}

fn retained_pane_layout<'a>(
    app: &app::App,
    cache: &'a mut HashMap<(usize, u16, u16), Option<RetainedPaneLayout>>,
    workspace_id: &shepr_protocol::WorkspaceId,
    width: u16,
    height: u16,
) -> Option<&'a RetainedPaneLayout> {
    let key = (workspace_id.number(), width, height);
    cache
        .entry(key)
        .or_insert_with(|| {
            let workspace_index = app.resolve_workspace_id(workspace_id)?;
            let workspace = app.state.workspaces.get(workspace_index)?;
            let area = Rect::new(0, 0, width, height);
            let panes = app
                .state
                .pane_geometry_in(area)
                .visible_panes(workspace.layout(), workspace.zoomed());
            Some(RetainedPaneLayout {
                workspace_index,
                panes,
            })
        })
        .as_ref()
}

fn has_synchronized_pane(app: &app::App, panes: &[ResolvedRetainedPane<'_>]) -> bool {
    panes.iter().any(|pane| {
        app.state
            .runtime_for_pane_in_workspace(
                &app.terminal_runtimes,
                pane.workspace_index,
                pane.identity.pane_id,
            )
            .is_some_and(shepr_mux::pane::PaneRuntime::synchronized_output_active)
    })
}

impl HeadlessServer {
    /// Reports each retained-render fallback after the full renderer has
    /// recovered the surface, including recurring reasons.
    pub(super) fn report_retained_surface_fallback(&mut self) {
        let Some(reason) = self.retained_surface_fallback_reason.take() else {
            return;
        };
        let repeated = !self.retained_surface_fallbacks_reported.insert(reason);
        debug!(
            reason,
            repeated, "retained pane surface fell back to a full render"
        );
    }

    /// Applies terminal dirty rows to the committed origin-relative pane surface.
    /// Any presentation or geometry uncertainty falls back to the complete renderer.
    pub(super) fn render_retained_pane_surface_and_stream(
        &mut self,
        pty_sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> bool {
        macro_rules! fallback {
            ($reason:literal) => {{
                self.retained_surface_fallback_reason = Some($reason);
                return false;
            }};
        }
        macro_rules! success {
            ($reason:literal) => {{
                trace!(reason = $reason, "retained pane surface update succeeded");
                return true;
            }};
        }

        if pty_sources.is_empty()
            || self.app.full_redraw_pending
            || self.app.state.settings.reveal_hidden_cursor_for_cjk_ime
        {
            fallback!("unsafe_state");
        }
        let mut targets = render_targets(&self.clients);
        targets.retain(|target| {
            self.clients
                .get(&target.client_id)
                .is_some_and(ClientConnection::is_active_shell_client)
        });
        if targets.is_empty() {
            success!("no_active_surface");
        }

        let mut recipients = Vec::with_capacity(targets.len());
        // Several clients can view the same workspace at the same size. The
        // layout only depends on that workspace and frame geometry, so compute
        // it once per key during this fanout pass and validate each client's
        // committed surface against the shared result.
        let mut layouts = HashMap::new();
        for target in &targets {
            let Some(client) = self.clients.get(&target.client_id) else {
                fallback!("client_missing");
            };
            if client.deferred_render() != RenderDemand::None {
                continue;
            }
            if client.render_state.requires_recompute() {
                fallback!("recompute_pending");
            }
            let Some(surface) = client.render_state.last_pane_surface() else {
                fallback!("no_baseline");
            };
            if surface.boot_id != self.client_shell_boot_id
                || surface.projection_revision != client.shell_state().projection_revision
                || surface.frame.width != target.terminal_size.cols.get()
                || surface.frame.height != target.terminal_size.rows.get()
            {
                fallback!("baseline_mismatch");
            }
            let identities = &client.surface_pane_identities;
            let layout = if let Some(identity) = identities.first() {
                let Some(layout) = retained_pane_layout(
                    &self.app,
                    &mut layouts,
                    &identity.workspace_id,
                    surface.frame.width,
                    surface.frame.height,
                ) else {
                    fallback!("baseline_mismatch");
                };
                Some(layout)
            } else {
                None
            };
            let panes = match layout {
                Some(layout) => {
                    let Some(panes) =
                        resolve_retained_panes(&self.app, surface, identities, layout)
                    else {
                        fallback!("baseline_mismatch");
                    };
                    panes
                }
                None if surface.panes.is_empty() => Vec::new(),
                None => fallback!("baseline_mismatch"),
            };
            if has_synchronized_pane(&self.app, &panes) {
                fallback!("synchronized_visible");
            }
            recipients.push(RetainedRecipient {
                client_id: target.client_id,
                surface,
                panes,
            });
        }
        if recipients.is_empty() {
            success!("all_recipients_deferred");
        }

        let mut collected = Vec::with_capacity(pty_sources.len());
        for source in pty_sources {
            let mut source_pane = None;
            let mut width = 0u16;
            let mut height = 0u16;
            for recipient in &recipients {
                let Some(pane) = recipient
                    .panes
                    .iter()
                    .find(|pane| pane.identity.pane_id == *source)
                else {
                    continue;
                };
                source_pane.get_or_insert((
                    (*pane.identity).clone(),
                    pane.workspace_index,
                    pane.identity.pane_id,
                ));
                width = width.max(pane.pane.inner_rect.width);
                height = height.max(pane.pane.inner_rect.height);
            }
            let Some((identity, workspace_index, pane_id)) = source_pane else {
                continue;
            };
            let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                workspace_index,
                pane_id,
            ) else {
                fallback!("runtime_missing");
            };
            let Some(snapshot) = runtime.collect_dirty_patch_snapshot(width, height) else {
                fallback!("terminal_snapshot");
            };
            let patch = match snapshot.patch {
                shepr_mux::pane::TerminalDirtyPatchOutcome::Clean => {
                    shepr_mux::pane::TerminalDirtyPatch { rows: Vec::new() }
                }
                shepr_mux::pane::TerminalDirtyPatchOutcome::Patch(patch) => patch,
                shepr_mux::pane::TerminalDirtyPatchOutcome::Fallback => {
                    fallback!("terminal_patch");
                }
            };
            collected.push(CollectedPanePatch {
                identity,
                patch,
                content_revision: snapshot.content_revision,
                scroll_metrics: snapshot.scroll_metrics,
                mouse_reporting: snapshot.mouse_reporting,
                sgr_pixel_mouse: snapshot.sgr_pixel_mouse,
                alternate_screen_active: snapshot.alternate_screen_active,
            });
        }

        let mut updates = Vec::with_capacity(recipients.len());
        for recipient in &recipients {
            let client_id = recipient.client_id;
            let surface = recipient.surface;
            let mut panes = surface.panes.clone();
            let projection_revision = surface.projection_revision;
            let base_surface_revision = surface.surface_revision;
            let mut changed_panes = Vec::with_capacity(collected.len());
            let mut patch_rows = Vec::new();
            let mut metadata_changed = false;
            for collected_pane in &collected {
                let Some((pane_index, _)) = recipient
                    .panes
                    .iter()
                    .enumerate()
                    .find(|(_, pane)| pane.identity == &collected_pane.identity)
                else {
                    continue;
                };
                let Some(pane) = panes.get_mut(pane_index) else {
                    fallback!("baseline_mismatch");
                };
                // Alternate-screen transitions change whether the pane reserves
                // a scrollbar gutter. Recompute layout and resize the runtime
                // through the complete renderer before retaining further rows.
                if pane.alternate_screen_active != collected_pane.alternate_screen_active {
                    fallback!("alternate_screen_geometry");
                }
                if patch_intersects_hyperlinks(
                    &surface.frame,
                    pane.inner_rect,
                    &collected_pane.patch,
                ) {
                    fallback!("hyperlink");
                }
                let previous_pane = pane.clone();
                let Some(rows) =
                    changed_rows(&surface.frame, pane.inner_rect, &collected_pane.patch)
                else {
                    fallback!("invalid_patch");
                };
                patch_rows.extend(rows);
                let Some(scrollbar_rows) = retained_scrollbar_patch(
                    &self.app,
                    &surface.frame,
                    pane,
                    recipient.panes[pane_index].reserved_scrollbar_gutter,
                    collected_pane.alternate_screen_active,
                    collected_pane.scroll_metrics,
                ) else {
                    fallback!("scrollbar_patch");
                };
                patch_rows.extend(scrollbar_rows);
                pane.content_revision = collected_pane.content_revision;
                pane.mouse_reporting = collected_pane.mouse_reporting;
                pane.sgr_pixel_mouse = collected_pane.sgr_pixel_mouse;
                pane.alternate_screen_active = collected_pane.alternate_screen_active;
                pane.scroll = collected_pane.scroll_metrics.map(|metrics| {
                    shepr_protocol::PaneSurfaceScrollMetrics {
                        offset_from_bottom: metrics.offset_from_bottom as u64,
                        max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                        viewport_rows: metrics.viewport_rows as u64,
                        history_origin: metrics.history_origin,
                    }
                });
                metadata_changed |= *pane != previous_pane;
                changed_panes.push(pane.clone());
            }

            // Panes and scrollbars were collected pane by pane; clients accept only
            // sorted, disjoint spans, so order them and send a full surface instead of
            // a patch they would reject.
            shepr_protocol::sort_patch_rows(&mut patch_rows);
            if shepr_protocol::validate_patch_rows(
                surface.frame.width,
                surface.frame.height,
                &patch_rows,
            )
            .is_err()
            {
                fallback!("invalid_patch");
            }
            let cursor = retained_cursor(&self.app, &recipient.panes);
            let cursor_changed = cursor != surface.frame.cursor;
            let patch = shepr_protocol::PaneSurfacePatch {
                boot_id: self.client_shell_boot_id.clone(),
                projection_revision,
                base_surface_revision,
                surface_revision: shepr_protocol::SurfaceRevision::new(0),
                rows: patch_rows,
                panes: changed_panes,
                cursor,
            };
            if patch.rows.is_empty() && !cursor_changed && !metadata_changed {
                continue;
            }
            updates.push(RetainedRecipientUpdate { client_id, patch });
        }
        if updates.is_empty() {
            success!("unchanged");
        }
        if recipients
            .iter()
            .any(|recipient| has_synchronized_pane(&self.app, &recipient.panes))
        {
            fallback!("synchronized_during_patch");
        }

        let mut sent = 0u64;
        let mut disconnected = Vec::new();
        for update in updates {
            let RetainedRecipientUpdate { client_id, patch } = update;
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            let Some(writer) = client.writer.as_ref().cloned() else {
                client.defer_full_render();
                continue;
            };
            let Some(prepared) = client.render_state.prepare_pane_surface_patch(patch) else {
                client.defer_full_render();
                continue;
            };
            let serialized = match Self::frame_server_message(prepared.message()) {
                Ok(serialized) => serialized,
                Err(error) => {
                    warn!(
                        ?client_id,
                        %error,
                        "failed to serialize retained pane surface patch"
                    );
                    client.request_repaint();
                    client.defer_full_render();
                    continue;
                }
            };
            let send = writer.render.try_send(serialized);
            match send {
                Ok(()) => {
                    client.clear_deferred_render();
                    client.render_state.commit_sent_frame(prepared);
                    if client.render_state.last_pane_surface().is_none() {
                        client.request_repaint();
                    }
                    sent += 1;
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.defer_full_render();
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    disconnected.push(client_id);
                }
            }
        }
        for client_id in disconnected {
            self.remove_client_and_resize_if_needed(client_id);
        }
        if sent > 0 {
            success!("sent");
        }
        success!("recovery_queued");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::WorkspaceFixture as _;

    fn cell(symbol: &str) -> shepr_protocol::CellData {
        shepr_protocol::CellData {
            symbol: symbol.to_owned(),
            fg: shepr_protocol::WireColor::Reset,
            bg: shepr_protocol::WireColor::Reset,
            style: shepr_protocol::WireStyle::default(),
            skip: false,
            hyperlink: None,
        }
    }

    #[test]
    fn retained_resolution_uses_the_typed_baseline_identity() {
        let mut app = app::App::new(
            &shepr_config::Config::default(),
            app::AppPolicy::Test,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        let workspace = shepr_mux::workspace::Workspace::test_new("typed-baseline");
        let pane_id = workspace.root_pane();
        app.state.workspaces.push(workspace);
        let workspace_id = app.state.workspaces[0].id.clone();
        let wire_workspace_id =
            shepr_protocol::WorkspaceId::from_number(999).expect("test workspace id");
        let surface = shepr_protocol::PaneSurfaceFrame {
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            projection_revision: shepr_protocol::ProjectionRevision::new(1),
            surface_revision: shepr_protocol::SurfaceRevision::new(1),
            frame: FrameData::blank(1, 1),
            panes: vec![shepr_protocol::PaneSurfacePane {
                pane_id: shepr_protocol::PublicPaneId::new(&wire_workspace_id, 1),
                content_revision: 0,
                rect: shepr_protocol::SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                inner_rect: shepr_protocol::SurfaceRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                scrollbar_rect: None,
                scroll: None,
                focused: true,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }],
            splits: Vec::new(),
        };
        let identities = vec![ClientPaneIdentity {
            workspace_id,
            pane_id,
        }];

        let mut layouts = HashMap::new();
        let layout = retained_pane_layout(
            &app,
            &mut layouts,
            &identities[0].workspace_id,
            surface.frame.width,
            surface.frame.height,
        )
        .expect("test workspace layout resolves");
        let resolved = resolve_retained_panes(&app, &surface, &identities, layout)
            .expect("typed identity resolves without parsing the wire id");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].workspace_index, 0);
        assert_eq!(resolved[0].identity.pane_id, pane_id);
    }

    #[test]
    fn retained_scrollbar_does_not_invent_a_gutter_at_the_pane_border() {
        let mut app = app::App::new(
            &shepr_config::Config::default(),
            app::AppPolicy::Test,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        app.state.settings.pane_borders = shepr_config::PaneBordersConfig::Always;
        app.state.settings.pane_scrollbars = true;
        app.state.settings.pane_outer_borders = true;
        let workspace = shepr_mux::workspace::Workspace::test_new("narrow-scrollbar");
        let workspace_id = workspace.id.clone();
        let pane_id = workspace.root_pane();
        app.state.workspaces.push(workspace);
        let area = Rect::new(0, 0, 6, 5);
        let layout = app.state.pane_geometry_in(area).visible_panes(
            app.state.workspaces[0].layout(),
            app.state.workspaces[0].zoomed(),
        );
        let pane_layout = layout.first().expect("test workspace has one pane");
        let pane_inner =
            shepr_mux::workspace::pane_inner_rect(pane_layout.rect, pane_layout.borders);
        assert_eq!(pane_inner.width, 4);
        let content = shepr_mux::workspace::terminal_content_rect(pane_inner, true, false);
        let mut pane = shepr_protocol::PaneSurfacePane {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            content_revision: 0,
            rect: pane_layout.rect.into(),
            inner_rect: content.into(),
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        };
        let surface = shepr_protocol::PaneSurfaceFrame {
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            projection_revision: shepr_protocol::ProjectionRevision::new(1),
            surface_revision: shepr_protocol::SurfaceRevision::new(1),
            frame: FrameData::blank(6, 5),
            panes: vec![pane.clone()],
            splits: Vec::new(),
        };
        let identities = vec![ClientPaneIdentity {
            workspace_id,
            pane_id,
        }];
        let mut layouts = HashMap::new();
        let layout = retained_pane_layout(
            &app,
            &mut layouts,
            &identities[0].workspace_id,
            surface.frame.width,
            surface.frame.height,
        )
        .expect("test workspace layout resolves");
        let resolved = resolve_retained_panes(&app, &surface, &identities, layout)
            .expect("the committed pane geometry matches its layout");
        assert_eq!(resolved[0].reserved_scrollbar_gutter, None);
        let metrics = shepr_mux::pane::ScrollMetrics {
            offset_from_bottom: 1,
            max_offset_from_bottom: 4,
            viewport_rows: 3,
            history_origin: shepr_vt::AbsRow(1),
        };

        let rows = retained_scrollbar_patch(
            &app,
            &surface.frame,
            &mut pane,
            resolved[0].reserved_scrollbar_gutter,
            false,
            Some(metrics),
        )
        .expect("a pane without a reserved gutter needs no scrollbar patch");

        assert!(rows.is_empty());
        assert_eq!(pane.scrollbar_rect, None);
    }

    #[test]
    fn retained_layout_is_reused_for_recipients_with_the_same_workspace_and_size() {
        let mut app = app::App::new(
            &shepr_config::Config::default(),
            app::AppPolicy::Test,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        let workspace = shepr_mux::workspace::Workspace::test_new("retained-layout-cache");
        let workspace_id = workspace.id.clone();
        app.state.workspaces.push(workspace);
        let mut cache = HashMap::new();

        let (first_panes, first_len) = {
            let first = retained_pane_layout(&app, &mut cache, &workspace_id, 80, 24)
                .expect("first recipient layout");
            (first.panes.as_ptr(), first.panes.len())
        };
        let (second_panes, second_len) = {
            let second = retained_pane_layout(&app, &mut cache, &workspace_id, 80, 24)
                .expect("second recipient layout");
            (second.panes.as_ptr(), second.panes.len())
        };

        assert_eq!(cache.len(), 1);
        assert_eq!(second_panes, first_panes);
        assert_eq!(second_len, first_len);

        retained_pane_layout(&app, &mut cache, &workspace_id, 81, 24)
            .expect("different frame size gets its own layout");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn retained_rows_send_only_changed_cell_spans() {
        let frame = FrameData {
            width: 6,
            height: 2,
            cells: vec![cell(" "); 12],
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let patch = shepr_mux::pane::TerminalDirtyPatch {
            rows: vec![(0, vec![cell(" "), cell("x"), cell("y"), cell(" ")])],
        };

        let rows = changed_rows(
            &frame,
            shepr_protocol::SurfaceRect {
                x: 1,
                y: 1,
                width: 4,
                height: 1,
            },
            &patch,
        )
        .expect("valid patch");

        assert_eq!(
            rows,
            vec![shepr_protocol::PaneSurfacePatchRow {
                x: 2,
                y: 1,
                cells: vec![cell("x"), cell("y"), cell(" ")],
            }]
        );
        assert_eq!(frame.cells, vec![cell(" "); 12], "planning must not commit");
    }

    #[test]
    fn retained_rows_include_the_cell_after_a_width_transition() {
        let frame = FrameData {
            width: 3,
            height: 1,
            cells: vec![cell("界"), cell("z"), cell("q")],
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let patch = shepr_mux::pane::TerminalDirtyPatch {
            rows: vec![(0, vec![cell("x"), cell("z"), cell("q")])],
        };

        let rows = changed_rows(
            &frame,
            shepr_protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: 3,
                height: 1,
            },
            &patch,
        )
        .expect("valid patch");

        assert_eq!(
            rows,
            vec![shepr_protocol::PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![cell("x"), cell("z")],
            }]
        );
    }

    #[test]
    fn retained_rows_omit_unchanged_full_dirty_rows() {
        let frame = FrameData {
            width: 4,
            height: 2,
            cells: vec![cell(" "); 8],
            cursor: None,
            hyperlinks: Vec::new(),
        };
        let patch = shepr_mux::pane::TerminalDirtyPatch {
            rows: vec![(0, vec![cell(" "); 4]), (1, vec![cell(" "); 4])],
        };

        let rows = changed_rows(
            &frame,
            shepr_protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            &patch,
        )
        .expect("valid patch");

        assert!(rows.is_empty());
    }
}
