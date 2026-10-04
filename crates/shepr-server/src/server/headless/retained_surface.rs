use super::*;
use crate::server::ClientId;
use crate::server::clients::ClientPaneIdentity;
use crate::server::committed_baseline::CommittedPane;
use crate::server::pane_surface::PaneSurfaceMetadata;
use shepr_mux::pane::PatchRow;
use tracing::trace;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum RetainedSurfaceFallback {
    ClientMissing,
    RecomputePending,
    NoBaseline,
    BaselineMismatch,
    RuntimeMissing,
    TerminalSnapshot(shepr_mux::pane::PatchUnavailable),
    AlternateScreenGeometry,
    Hyperlink,
    InvalidPatch,
    ScrollbarPatch,
    SynchronizedDuringPatch,
    PatchAdmission(crate::server::render_stream::PatchPreparationFailure),
}

fn rect_fits_frame(rect: shepr_protocol::SurfaceRect, frame: &FrameData) -> bool {
    rect.x.saturating_add(rect.width) <= frame.width()
        && rect.y.saturating_add(rect.height) <= frame.height()
}

fn patch_intersects_hyperlinks(
    frame: &FrameData,
    area: shepr_protocol::SurfaceRect,
    patch: &shepr_mux::pane::TerminalDirtyPatch,
) -> bool {
    if frame.hyperlinks().is_empty() || !rect_fits_frame(area, frame) {
        return false;
    }
    let width = usize::from(frame.width());
    patch
        .rows
        .iter()
        .filter(|row| row.y < area.height)
        .any(|row| {
            let start = usize::from(area.y + row.y) * width + usize::from(area.x);
            let end = start + usize::from(area.width);
            end > frame.cells().len()
                || frame.cells()[start..end]
                    .iter()
                    .any(|cell| cell.hyperlink.is_some())
        })
}

fn patch_row_changed(frame: &FrameData, row: &shepr_protocol::PaneSurfacePatchRow) -> Option<bool> {
    if row.y >= frame.height()
        || row.x.saturating_add(u16::try_from(row.cells.len()).ok()?) > frame.width()
    {
        return None;
    }
    let start = usize::from(row.y) * usize::from(frame.width()) + usize::from(row.x);
    let end = start + row.cells.len();
    if end > frame.cells().len() {
        return None;
    }
    Some(frame.cells()[start..end] != row.cells)
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
    for PatchRow { y: local_y, cells } in &patch.rows {
        if *local_y >= area.height {
            continue;
        }
        let width = usize::from(area.width);
        if cells.len() < width {
            return None;
        }
        let y = area.y + *local_y;
        let frame_start = usize::from(y) * usize::from(frame.width()) + usize::from(area.x);
        let frame_end = frame_start.checked_add(width)?;
        let existing = frame.cells().get(frame_start..frame_end)?;
        // The row was collected at the widest recipient's width; a narrower
        // cut can split a pair the collection kept whole, so it gets the
        // same rule a full render at this width applies. The shared row is
        // left as it is for the wider recipients. Only whole rows are
        // normalized: a span cut out of one below can start with a tail or
        // end with a lead whose other half is unchanged in the baseline,
        // which is valid once applied, so spans must never go through it.
        let recut;
        let desired = if shepr_surface::pane_row::pane_row_is_normalized(&cells[..width]) {
            &cells[..width]
        } else {
            let mut row = cells[..width].to_vec();
            shepr_surface::pane_row::normalize_pane_row(&mut row);
            recut = row;
            &recut[..]
        };
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

/// The rows that bring the pane's scrollbar column in `frame` to what the ui
/// says it looks like with `metrics`, and the committed pane's track updated
/// to match. `look` is the pane as laid out against the baseline.
fn retained_scrollbar_patch(
    app: &app::App,
    frame: &FrameData,
    pane: &mut shepr_protocol::PaneSurfacePane,
    look: &crate::ui::PaneSurface,
    metrics: Option<shepr_mux::pane::ScrollMetrics>,
) -> Option<Vec<shepr_protocol::PaneSurfacePatchRow>> {
    let look = look.clone().with_scroll(metrics);
    let paint = look.scrollbar_paint(pane.scrollbar_rect, app.state(), metrics);
    pane.scrollbar_rect = look
        .scrollbar_rect
        .map(shepr_surface::ratatui_conversion::surface_rect);
    let Some((rect, cells)) = paint else {
        return Some(Vec::new());
    };
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
    panes: &[CommittedPane<'_>],
) -> Option<shepr_protocol::CursorState> {
    let pane = panes.iter().find(|pane| pane.wire.focused)?;
    let runtime = app.pane_runtime(pane.identity.pane_id)?;
    pane.look.cursor(app.state(), runtime)
}

struct RetainedRecipient<'a> {
    client_id: ClientId,
    surface: &'a shepr_protocol::PaneSurfaceFrame,
    // The committed panes with the typed identities committed beside them, for
    // source matching, synchronized-output checks, and cursor lookup.
    panes: Vec<CommittedPane<'a>>,
}

struct CollectedPanePatch {
    identity: ClientPaneIdentity,
    patch: shepr_mux::pane::TerminalDirtyPatch,
    metadata: PaneSurfaceMetadata,
}

struct RetainedRecipientUpdate {
    client_id: ClientId,
    patch: shepr_protocol::PaneSurfacePatch,
}

fn has_synchronized_pane(app: &app::App, panes: &[CommittedPane<'_>]) -> bool {
    panes.iter().any(|pane| {
        app.pane_runtime(pane.identity.pane_id)
            .is_some_and(|runtime| runtime.read().synchronized_output_active())
    })
}

/// What `render_patches` did with each candidate. A candidate whose patch
/// changed nothing is in none of these.
#[derive(Default)]
pub(super) struct PatchOutcome {
    /// Took a patch.
    pub(super) sent: Vec<ClientId>,
    /// Needs a full surface this pass instead.
    pub(super) promote: Vec<ClientId>,
    /// Its slot was busy; owed a full surface once it frees.
    pub(super) owed: Vec<ClientId>,
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
            ?reason,
            repeated, "retained pane surface fell back to a full render"
        );
    }

    /// Sends pending PTY damage to `ids` as retained patches, each judged on
    /// its own. A candidate whose slot is busy is owed (the damage is consumed
    /// for everyone, so its baseline is now behind). A candidate failing a
    /// check about its own baseline or patch, or viewing a pane whose dirty
    /// rows could not be collected, is promoted to the full step; the others
    /// still get their patches. Every recipient is planned before any is
    /// sent, and a client's baseline changes only on its own successful send.
    pub(super) fn render_patches(
        &mut self,
        ids: &[ClientId],
        pty_sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> PatchOutcome {
        let mut outcome = PatchOutcome::default();
        macro_rules! fallback {
            ($reason:expr, $id:expr, $label:lifetime) => {{
                self.retained_surface_fallback_reason.get_or_insert($reason);
                outcome.promote.push($id);
                continue $label;
            }};
        }
        let targets = render_targets(&self.clients)
            .filter(|target| ids.contains(&target.client_id))
            .collect::<Vec<_>>();
        // Check slots before baseline validation or source collection.
        let mut ready = Vec::new();
        for target in targets {
            if let Some(client) = self.clients.get_mut(&target.client_id) {
                if !client.outbox.surface_slot_free() {
                    client.render_state.owe();
                    outcome.owed.push(target.client_id);
                } else {
                    ready.push(target);
                }
            }
        }
        let targets = ready;
        let mut recipients = Vec::with_capacity(targets.len());
        // Several clients can view the same workspace at the same size. The
        // layout only depends on that workspace and frame geometry, so compute
        // it once per key during this fanout pass and validate each client's
        // committed surface against the shared result.
        let mut layouts = crate::ui::PaneLayoutCache::default();
        'targets: for target in &targets {
            let Some(client) = self.clients.get(&target.client_id) else {
                fallback!(RetainedSurfaceFallback::ClientMissing, target.client_id, 'targets);
            };
            if client.render_state.requires_recompute() {
                fallback!(RetainedSurfaceFallback::RecomputePending, target.client_id, 'targets);
            }
            let Some(baseline) = client.render_state.committed_baseline() else {
                fallback!(RetainedSurfaceFallback::NoBaseline, target.client_id, 'targets);
            };
            let surface = baseline.surface();
            if surface.boot_id != self.client_shell_boot_id
                || surface.projection_revision != client.shell_state().projection_revision
                || surface.frame.width() != target.terminal_size.cols.get()
                || surface.frame.height() != target.terminal_size.rows.get()
            {
                fallback!(RetainedSurfaceFallback::BaselineMismatch, target.client_id, 'targets);
            }
            let Some(panes) = baseline.panes(self.app.state(), &mut layouts) else {
                fallback!(RetainedSurfaceFallback::BaselineMismatch, target.client_id, 'targets);
            };
            recipients.push(RetainedRecipient {
                client_id: target.client_id,
                surface,
                panes,
            });
        }
        if recipients.is_empty() {
            return outcome;
        }

        let mut collected = Vec::with_capacity(pty_sources.len());
        let mut failed_sources = HashSet::new();
        macro_rules! source_fallback {
            ($reason:expr, $source:expr) => {{
                self.retained_surface_fallback_reason.get_or_insert($reason);
                failed_sources.insert(*$source);
                continue;
            }};
        }
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
                source_pane.get_or_insert(((*pane.identity).clone(), pane.identity.pane_id));
                width = width.max(pane.wire.inner_rect.width);
                height = height.max(pane.wire.inner_rect.height);
            }
            let Some((identity, pane_id)) = source_pane else {
                continue;
            };
            let Some(runtime) = self.app.pane_runtime(pane_id) else {
                source_fallback!(RetainedSurfaceFallback::RuntimeMissing, source);
            };
            let snapshot = match runtime.read().collect_dirty_patch_snapshot(width, height) {
                Ok(snapshot) => snapshot,
                Err(reason) => {
                    source_fallback!(RetainedSurfaceFallback::TerminalSnapshot(reason), source)
                }
            };
            let metadata = PaneSurfaceMetadata::from_dirty_snapshot(&snapshot);
            let patch = snapshot
                .patch
                .unwrap_or(shepr_mux::pane::TerminalDirtyPatch { rows: Vec::new() });
            collected.push(CollectedPanePatch {
                identity,
                patch,
                metadata,
            });
        }

        let mut updates = Vec::with_capacity(recipients.len());
        'recipients: for recipient in &recipients {
            let client_id = recipient.client_id;
            if recipient
                .panes
                .iter()
                .any(|pane| failed_sources.contains(&pane.identity.pane_id))
            {
                outcome.promote.push(client_id);
                continue;
            }
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
                    fallback!(RetainedSurfaceFallback::BaselineMismatch, client_id, 'recipients);
                };
                // Alternate-screen transitions change whether the pane reserves
                // a scrollbar gutter. Recompute layout and resize the runtime
                // through the complete renderer before retaining further rows.
                if pane.alternate_screen_active != collected_pane.metadata.alternate_screen_active {
                    fallback!(RetainedSurfaceFallback::AlternateScreenGeometry, client_id, 'recipients);
                }
                if patch_intersects_hyperlinks(
                    &surface.frame,
                    pane.inner_rect,
                    &collected_pane.patch,
                ) {
                    fallback!(RetainedSurfaceFallback::Hyperlink, client_id, 'recipients);
                }
                let previous_pane = pane.clone();
                let Some(rows) =
                    changed_rows(&surface.frame, pane.inner_rect, &collected_pane.patch)
                else {
                    fallback!(RetainedSurfaceFallback::InvalidPatch, client_id, 'recipients);
                };
                patch_rows.extend(rows);
                let Some(scrollbar_rows) = retained_scrollbar_patch(
                    &self.app,
                    &surface.frame,
                    pane,
                    &recipient.panes[pane_index].look,
                    collected_pane.metadata.scroll(),
                ) else {
                    fallback!(RetainedSurfaceFallback::ScrollbarPatch, client_id, 'recipients);
                };
                patch_rows.extend(scrollbar_rows);
                collected_pane.metadata.apply(pane);
                metadata_changed |= *pane != previous_pane;
                changed_panes.push(pane.clone());
            }

            // Collection is pane by pane; put spans in the row-major order required
            // by the shared baseline admission in prepare_pane_surface_patch below.
            shepr_protocol::sort_patch_rows(&mut patch_rows);
            let cursor = retained_cursor(&self.app, &recipient.panes);
            let cursor_changed = cursor.as_ref() != surface.frame.cursor();
            let patch = shepr_protocol::PaneSurfacePatch {
                boot_id: self.client_shell_boot_id.clone(),
                projection_revision,
                base_surface_revision,
                // Placeholder: `prepare_pane_surface_patch` assigns the real
                // revision before admitting the patch, and a patch it rejects
                // is dropped unsent. The client would also refuse any revision
                // that is not its baseline's exact successor.
                surface_revision: shepr_protocol::SurfaceRevision::ZERO,
                rows: patch_rows,
                panes: changed_panes,
                cursor,
            };
            if patch.rows.is_empty() && !cursor_changed && !metadata_changed {
                continue;
            }
            updates.push(RetainedRecipientUpdate { client_id, patch });
        }

        let synchronized = recipients
            .iter()
            .filter(|recipient| has_synchronized_pane(&self.app, &recipient.panes))
            .map(|recipient| recipient.client_id)
            .collect::<HashSet<_>>();
        drop(recipients);
        for update in updates {
            let RetainedRecipientUpdate { client_id, patch } = update;
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if synchronized.contains(&client_id) {
                self.retained_surface_fallback_reason
                    .get_or_insert(RetainedSurfaceFallback::SynchronizedDuringPatch);
                outcome.promote.push(client_id);
                continue;
            }
            let prepared = match client.render_state.prepare_pane_surface_patch(patch) {
                Ok(prepared) => prepared,
                Err(reason) => {
                    self.retained_surface_fallback_reason
                        .get_or_insert(RetainedSurfaceFallback::PatchAdmission(reason));
                    outcome.promote.push(client_id);
                    continue;
                }
            };
            let serialized = match shepr_protocol::encode_message(prepared.message()) {
                Ok(serialized) => serialized,
                Err(error) => {
                    warn!(
                        ?client_id,
                        %error,
                        "failed to serialize retained pane surface patch"
                    );
                    client.request_repaint();
                    outcome.promote.push(client_id);
                    continue;
                }
            };
            match client.outbox.offer_surface(serialized) {
                crate::server::outbox::SurfaceOffer::Queued => {
                    client.render_state.clear_debt();
                    client.render_state.commit_sent_frame(prepared);
                    if client.render_state.last_pane_surface().is_none() {
                        client.request_repaint();
                    }
                    outcome.sent.push(client_id);
                }
                // The slot was free when this pass checked it and only the
                // writer empties it, so this is a bug guard: owe, not panic.
                crate::server::outbox::SurfaceOffer::Occupied => {
                    client.render_state.owe();
                    outcome.owed.push(client_id);
                }
                // The outbox closed itself; the reap removes the client.
                crate::server::outbox::SurfaceOffer::Closed => {}
            }
        }

        trace!(
            sent = outcome.sent.len(),
            promoted = outcome.promote.len(),
            owed = outcome.owed.len(),
            "retained pane surface pass completed"
        );
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::committed_baseline::CommittedBaseline;
    use crate::test_support::WorkspaceFixture as _;

    fn cell(symbol: &str) -> shepr_protocol::CellData {
        shepr_protocol::CellData {
            symbol: symbol.to_owned(),
            grid_width: shepr_protocol::GridCellWidth::Grapheme,
            fg: shepr_protocol::WireColor::Reset,
            bg: shepr_protocol::WireColor::Reset,
            style: shepr_protocol::WireStyle::default(),
            hyperlink: None,
        }
    }

    fn test_frame(width: u16, height: u16, cells: Vec<shepr_protocol::CellData>) -> FrameData {
        FrameData::new(cells, width, height, None, Vec::new()).expect("test frame")
    }

    #[test]
    fn retained_resolution_uses_the_typed_baseline_identity() {
        let mut app = app::App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("typed-baseline");
        let pane_id = workspace.tree().root();
        app.test_state_mut().test_push_workspace(workspace);
        let workspace_id = app.state().ws(0).id();
        let wire_workspace_id =
            shepr_protocol::WorkspaceId::from_number(999).expect("test workspace id");
        let surface = shepr_protocol::PaneSurfaceFrame {
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            projection_revision: shepr_protocol::ProjectionRevision::FIRST,
            surface_revision: shepr_protocol::SurfaceRevision::FIRST,
            frame: FrameData::blank(1, 1).expect("test frame"),
            panes: vec![shepr_protocol::PaneSurfacePane {
                pane_id: shepr_protocol::PublicPaneId::new(
                    &wire_workspace_id,
                    shepr_protocol::PanePublicNumber::new(1).expect("nonzero literal"),
                ),
                content_revision: shepr_protocol::ContentRevision::default(),
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
                pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
                alternate_screen_active: false,
            }],
            splits: Vec::new(),
        };
        let identities = vec![ClientPaneIdentity {
            workspace_id,
            pane_id,
        }];

        let baseline = CommittedBaseline::new(surface, identities);
        let mut layouts = crate::ui::PaneLayoutCache::default();
        let resolved = baseline
            .panes(app.state(), &mut layouts)
            .expect("typed identity resolves without parsing the wire id");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].identity.workspace_id, workspace_id);
        assert_eq!(resolved[0].identity.pane_id, pane_id);
    }

    #[test]
    fn retained_scrollbar_does_not_invent_a_gutter_at_the_pane_border() {
        let mut app = app::App::new(&shepr_config::ServerConfig::default());
        app.test_state_mut().settings_mut().pane_borders = shepr_config::PaneBordersConfig::Always;
        app.test_state_mut().settings_mut().pane_scrollbars = true;
        app.test_state_mut().settings_mut().pane_outer_borders = true;
        let workspace = shepr_mux::workspace::Workspace::test_new("narrow-scrollbar");
        let workspace_id = workspace.id();
        let pane_id = workspace.tree().root();
        app.test_state_mut().test_push_workspace(workspace);
        let area = shepr_core::geometry::Rect::new(0, 0, 6, 5);
        let layout = app.state().chrome_in(area).visible_panes(
            app.state().ws(0).tree().layout(),
            app.state().ws(0).tree().zoomed(),
        );
        let pane_layout = layout.first().expect("test workspace has one pane");
        let pane_inner = pane_layout.inner_rect();
        assert_eq!(pane_inner.width, 4);
        let content = shepr_core::chrome::content_rect(pane_inner, true, false);
        let mut pane = shepr_protocol::PaneSurfacePane {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            content_revision: shepr_protocol::ContentRevision::default(),
            rect: pane_layout.rect,
            inner_rect: content,
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
            alternate_screen_active: false,
        };
        let surface = shepr_protocol::PaneSurfaceFrame {
            boot_id: shepr_test_fixtures::fixed_boot_id(1),
            projection_revision: shepr_protocol::ProjectionRevision::FIRST,
            surface_revision: shepr_protocol::SurfaceRevision::FIRST,
            frame: FrameData::blank(6, 5).expect("test frame"),
            panes: vec![pane.clone()],
            splits: Vec::new(),
        };
        let identities = vec![ClientPaneIdentity {
            workspace_id,
            pane_id,
        }];
        let baseline = CommittedBaseline::new(surface.clone(), identities);
        let mut layouts = crate::ui::PaneLayoutCache::default();
        let resolved = baseline
            .panes(app.state(), &mut layouts)
            .expect("the committed pane geometry matches its layout");
        assert_eq!(resolved[0].look.scrollbar_gutter, None);
        let metrics = shepr_mux::pane::ScrollMetrics::new(1, 4, 3, shepr_vt::AbsRow(1));

        let rows = retained_scrollbar_patch(
            &app,
            &surface.frame,
            &mut pane,
            &resolved[0].look,
            Some(metrics),
        )
        .expect("a pane without a reserved gutter needs no scrollbar patch");

        assert!(rows.is_empty());
        assert_eq!(pane.scrollbar_rect, None);
    }

    #[test]
    fn retained_layout_is_reused_for_recipients_with_the_same_workspace_and_size() {
        let mut app = app::App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("retained-layout-cache");
        let workspace_id = workspace.id();
        app.test_state_mut().test_push_workspace(workspace);
        let mut cache = crate::ui::PaneLayoutCache::default();

        let (first_panes, first_len) = {
            let first = cache
                .chromes(app.state(), workspace_id, 80, 24)
                .expect("first recipient layout");
            (first.as_ptr(), first.len())
        };
        let (second_panes, second_len) = {
            let second = cache
                .chromes(app.state(), workspace_id, 80, 24)
                .expect("second recipient layout");
            (second.as_ptr(), second.len())
        };

        assert_eq!(cache.len(), 1);
        assert_eq!(second_panes, first_panes);
        assert_eq!(second_len, first_len);

        cache
            .chromes(app.state(), workspace_id, 81, 24)
            .expect("different frame size gets its own layout");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn retained_rows_send_only_changed_cell_spans() {
        let frame = test_frame(6, 2, vec![cell(" "); 12]);
        let patch = shepr_mux::pane::TerminalDirtyPatch {
            rows: vec![PatchRow {
                y: 0,
                cells: vec![cell(" "), cell("x"), cell("y"), cell(" ")],
            }],
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
        assert_eq!(
            frame.cells(),
            vec![cell(" "); 12],
            "planning must not commit"
        );
    }

    #[test]
    fn retained_rows_include_the_cell_after_a_width_transition() {
        let frame = test_frame(3, 1, vec![cell("界"), cell("z"), cell("q")]);
        let patch = shepr_mux::pane::TerminalDirtyPatch {
            rows: vec![PatchRow {
                y: 0,
                cells: vec![cell("x"), cell("z"), cell("q")],
            }],
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
    fn a_narrower_recipient_gets_a_blank_where_the_shared_row_holds_a_wide_glyph() {
        let pane_cell = |symbol: &str, grid_width| shepr_protocol::CellData {
            grid_width,
            ..cell(symbol)
        };
        let one = shepr_protocol::GridCellWidth::One;
        let lead = shepr_protocol::GridCellWidth::WideLead;
        let tail = shepr_protocol::GridCellWidth::WideTail;
        // Collected once at the wider recipient's width: the pair is whole.
        let patch = shepr_mux::pane::TerminalDirtyPatch {
            rows: vec![PatchRow {
                y: 0,
                cells: vec![
                    pane_cell("a", one),
                    pane_cell("\u{754c}", lead),
                    pane_cell("", tail),
                    pane_cell("b", one),
                ],
            }],
        };
        let frame =
            |width: u16| test_frame(width, 1, vec![pane_cell(" ", one); usize::from(width)]);
        let area = |width| shepr_protocol::SurfaceRect {
            x: 0,
            y: 0,
            width,
            height: 1,
        };

        let wide = changed_rows(&frame(4), area(4), &patch).expect("valid patch");
        let wide_cells: Vec<_> = wide.iter().flat_map(|row| row.cells.clone()).collect();
        assert!(wide_cells.iter().any(|cell| cell.grid_width == lead));

        let narrow = changed_rows(&frame(2), area(2), &patch).expect("valid patch");
        let mut applied = frame(2);
        for row in &narrow {
            let start = usize::from(row.x);
            applied.cells_mut()[start..start + row.cells.len()].clone_from_slice(&row.cells);
        }
        assert_eq!(applied.cells()[0].symbol, "a");
        assert_eq!(applied.cells()[1].symbol, " ");
        assert_eq!(applied.cells()[1].grid_width, one);
        assert!(shepr_surface::pane_row::pane_row_is_normalized(
            applied.cells()
        ));
        // The shared row is untouched for the wider recipient.
        assert_eq!(patch.rows[0].cells[1].grid_width, lead);
    }

    #[test]
    fn retained_rows_omit_unchanged_full_dirty_rows() {
        let frame = test_frame(4, 2, vec![cell(" "); 8]);
        let patch = shepr_mux::pane::TerminalDirtyPatch {
            rows: vec![
                PatchRow {
                    y: 0,
                    cells: vec![cell(" "); 4],
                },
                PatchRow {
                    y: 1,
                    cells: vec![cell(" "); 4],
                },
            ],
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
