use crate::shell::presentation::surfaces::PatchRejection;
use crate::shell::state::ClientShellState;

pub(crate) struct ClientComposedSurfacePatch {
    pub(crate) rows: Vec<shepr_protocol::PaneSurfacePatchRow>,
    pub(crate) cursor: Option<shepr_protocol::CursorState>,
}

pub(crate) enum ClientPaneSurfacePatchOutcome {
    /// The patch does not follow the reader baseline. Pairing never rejects a patch.
    Rejected(PatchRejection),
    Applied(PatchPresentation),
}

pub(crate) enum PatchPresentation {
    /// Only the baseline advanced; the last presented pair is unchanged.
    Held,
    /// The presented pair changed and needs full composition.
    Compose,
    /// The presented pair changed and only these composed rows need writing.
    Rows(ClientComposedSurfacePatch),
}

fn patch_updates_pane<'a>(
    mut patched_pane_ids: impl Iterator<Item = &'a shepr_protocol::PublicPaneId>,
    pane_id: &shepr_protocol::PublicPaneId,
) -> bool {
    patched_pane_ids.any(|patched| patched == pane_id)
}

fn copy_mode_cursor_changed_on_owner(
    state: &ClientShellState,
    patch: &shepr_protocol::PaneSurfacePatch,
) -> bool {
    let Some(copy_mode) = state.copy.as_ref() else {
        return false;
    };
    let Some(surface) = state.pane_surface() else {
        return false;
    };
    patch.cursor.as_ref() != surface.frame.cursor()
        && surface
            .panes
            .iter()
            .any(|pane| pane.focused && pane.pane_id == copy_mode.pane_id)
}

fn fast_path_blocker(
    state: &ClientShellState,
    patch: &shepr_protocol::PaneSurfacePatch,
    area: ratatui::layout::Rect,
) -> bool {
    // The last composition drew over pane cells (notices, banners, overlays, the mode
    // bar, a clipped surface) or replaced the pane cursor (copy mode, an overlay, an
    // unusable endpoint). Patch rows would overwrite those effects, so they compose.
    let composition = state.presentation.composition();
    let composition_covers_panes =
        composition.pane_cells_occluded || composition.pane_cursor_overridden;
    // A surface produced for a larger pane area (before a resize or sidebar toggle took
    // effect) is drawn clipped by `compose`. Its patch rows, offset into this layout, could
    // land on the mode bar or past the frame, so they go through compose too. The layout
    // can change after the last composition, so this reads the current area.
    let surface_overflows = state
        .pane_surface()
        .is_some_and(|surface| crate::shell::view::resolve::surface_overflows_area(surface, area));
    // Selection and copy mode affect pane cells only when their owner is patched. A parked
    // copy session must not send unrelated pane output through full-frame composition.
    let selection_patched = state
        .mouse_selection
        .selection
        .as_ref()
        .is_some_and(|selection| {
            selection.is_visible()
                && patch_updates_pane(
                    patch.panes.iter().map(|pane| &pane.pane_id),
                    selection.pane_id(),
                )
        });
    // The cursor is sampled independently of the changed pane list, so a patch can move it
    // without naming its owner in metadata. Recompose only when that owner has copy state.
    let copy_mode_patched = state.copy.as_ref().is_some_and(|copy_mode| {
        patch_updates_pane(
            patch.panes.iter().map(|pane| &pane.pane_id),
            &copy_mode.pane_id,
        )
    }) || copy_mode_cursor_changed_on_owner(state, patch);
    let unknown_pane = patch.panes.iter().any(|pane| {
        !state
            .pane_hits()
            .iter()
            .any(|hit| hit.pane_id == pane.pane_id)
    });
    composition_covers_panes
        || surface_overflows
        || selection_patched
        || copy_mode_patched
        || unknown_pane
}

impl ClientShellState {
    /// A patch from the shown connection `generation`; it must follow that
    /// connection's own baseline.
    pub(crate) fn apply_pane_surface_patch_from(
        &mut self,
        patch: &shepr_protocol::PaneSurfacePatch,
        generation: shepr_protocol::ConnectionGeneration,
    ) -> ClientPaneSurfacePatchOutcome {
        if let Err(reason) = self.presentation.surfaces.validate(patch, generation) {
            return ClientPaneSurfacePatchOutcome::Rejected(reason);
        }
        if !self.presentation.surfaces.is_paired() {
            return match self.presentation.surfaces.apply_validated(patch) {
                Ok(()) => ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held),
                Err(reason) => ClientPaneSurfacePatchOutcome::Rejected(reason),
            };
        }
        let (cols, rows) = self.view().map_or_else(Default::default, |view| view.size);
        let area = self.layout(cols, rows).pane_surface;
        // Composition metadata can be stale between a shell change and its redraw. A
        // change that needs recomposition leaves presentation dirty, so the client drops
        // any `Rows` result and composes the current shell before writing it.
        let fast_path_blocker = fast_path_blocker(self, patch, area);
        let fast_path_area = (!fast_path_blocker).then_some(area);
        let composed_patch = fast_path_area.map(|area| ClientComposedSurfacePatch {
            rows: patch
                .rows
                .iter()
                .map(|row| shepr_protocol::PaneSurfacePatchRow {
                    x: area.x.saturating_add(row.x),
                    y: area.y.saturating_add(row.y),
                    cells: row.cells.clone(),
                })
                .collect(),
            cursor: patch
                .cursor
                .clone()
                .map(|cursor| shepr_protocol::CursorState {
                    x: area.x.saturating_add(cursor.x),
                    y: area.y.saturating_add(cursor.y),
                    visible: cursor.visible,
                    shape: cursor.shape,
                }),
        });
        if let Some(area) = fast_path_area {
            if let Err(reason) = self.presentation.surfaces.apply_validated(patch) {
                return ClientPaneSurfacePatchOutcome::Rejected(reason);
            }
            // The hits change here, before the rows are written to the host, and stay
            // changed if that write fails. That is deliberate, unlike a full frame's
            // commit (which waits for the write). A patch cannot move, resize or refocus a
            // pane (`validate` rejects a geometry change), so the only hit fields it
            // changes are the pane's scrollbar gutter and scroll metrics and its mouse
            // reporting and pixel mouse modes. Those describe the pane as the server now
            // runs it (the server draws the scrollbar into the pane surface itself), not
            // client chrome: input is delivered against the server's pane, so a click
            // after a failed write must already follow the mode the child turned on, and
            // a scroll must start from the server's offset. Notices, reveals and other
            // shell state the full-frame commit guards do not change here. The surfaces
            // the hits mirror were applied just above for the same reason (later patches
            // build on them), and a failed write makes the next frame a full repaint,
            // which draws exactly this state.
            for updated in &patch.panes {
                if !self.presentation.patch_pane_hit(updated, area) {
                    continue;
                }
                self.scroll_target_shown(&updated.pane_id, updated.scroll);
            }
        } else {
            let before = self
                .mouse_selection
                .facts_in(self.presentation.surfaces.paired());
            if let Err(reason) = self.presentation.surfaces.apply_validated(patch) {
                return ClientPaneSurfacePatchOutcome::Rejected(reason);
            }
            if let Some(surface) = self.presentation.surfaces.paired() {
                crate::shell::transitions::surface_presented(
                    &mut self.mouse_selection,
                    &mut self.copy,
                    &mut self.scroll_lanes,
                    &mut self.ledger,
                    before,
                    surface,
                );
            }
        }
        ClientPaneSurfacePatchOutcome::Applied(match composed_patch {
            Some(c) => PatchPresentation::Rows(c),
            None => PatchPresentation::Compose,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::shell::config::ClientShellConfig;
    use ratatui::buffer::Buffer;
    use shepr_config::ClientConfig;
    use shepr_protocol::FrameData;
    use shepr_surface::ratatui_conversion::FrameDataExt as _;

    use crate::shell::copy::{CopyEntry, CopySession};
    use crate::shell::state::ClientShellState;

    use super::{fast_path_blocker, patch_updates_pane};
    use ratatui::layout::Rect;

    fn cursor(x: u16) -> shepr_protocol::CursorState {
        shepr_protocol::CursorState {
            x,
            y: 0,
            visible: true,
            shape: shepr_protocol::CursorShapeParam::SteadyBlock,
        }
    }

    fn test_surface_pane(
        pane_id: shepr_protocol::PublicPaneId,
        focused: bool,
        rect: shepr_protocol::SurfaceRect,
    ) -> shepr_protocol::PaneSurfacePane {
        shepr_protocol::PaneSurfacePane {
            pane_id,
            content_revision: shepr_protocol::ContentRevision::default(),
            rect,
            inner_rect: rect,
            scrollbar_rect: None,
            scroll: None,
            focused,
            mouse_reporting: false,
            pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
            alternate_screen_active: false,
        }
    }

    fn state_with_copy_pane_focus(copy_pane_focused: bool) -> (ClientShellState, Rect) {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.presentation.set_composed_size((80, 24));
        let mut snapshot = crate::shell::tests::snapshot();
        let copy_pane_id = snapshot.panes[0].pane_id;
        let other_pane_id = crate::tests::test_pane_id("w1:p2");
        if !copy_pane_focused {
            snapshot.focused_pane_id = Some(other_pane_id);
            let mut other_pane = snapshot.panes[0].clone();
            other_pane.pane_id = other_pane_id;
            snapshot.panes.push(other_pane);
        }
        state.set_snapshot(Box::new(snapshot));
        let area = state.layout(80, 24).pane_surface;
        let copy_rect = shepr_protocol::SurfaceRect {
            x: 0,
            y: 0,
            width: if copy_pane_focused {
                area.width
            } else {
                area.width / 2
            },
            height: area.height,
        };
        let panes = if copy_pane_focused {
            vec![test_surface_pane(copy_pane_id, true, copy_rect)]
        } else {
            let other_rect = shepr_protocol::SurfaceRect {
                x: copy_rect.width,
                y: 0,
                width: area.width.saturating_sub(copy_rect.width),
                height: area.height,
            };
            vec![
                test_surface_pane(copy_pane_id, false, copy_rect),
                test_surface_pane(other_pane_id, true, other_rect),
            ]
        };
        let snapshot = state
            .endpoints
            .active
            .snapshot()
            .expect("snapshot installed");
        let buffer = Buffer::empty(Rect::new(0, 0, area.width, area.height));
        state.receive_pane_surface_from(
            shepr_protocol::PaneSurfaceFrame {
                boot_id: snapshot.boot_id.clone(),
                projection_revision: snapshot.revision,
                surface_revision: shepr_protocol::SurfaceRevision::FIRST,
                frame: FrameData::from_ratatui_buffer_with_hyperlinks(
                    &buffer,
                    Some(cursor(1)),
                    &[],
                )
                .expect("test buffer is a valid frame"),
                panes,
                splits: Vec::new(),
            },
            state
                .endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        );
        state.copy = Some(CopySession::start(CopyEntry {
            pane_id: copy_pane_id,
            scroll: shepr_term::ScrollMetrics::new(
                0,
                0,
                usize::from(area.height),
                shepr_term::AbsRow(0),
            ),
            geometry: (area.width, area.height),
            alternate_screen_active: false,
            cursor: shepr_protocol::command::PaneTextPoint {
                row: shepr_term::AbsRow(0),
                col: 0,
            },
            rows: crate::shell::ledger::Ticket::fixture(1),
        }));
        (state, area)
    }

    fn cursor_patch(
        state: &ClientShellState,
        cursor: Option<shepr_protocol::CursorState>,
    ) -> shepr_protocol::PaneSurfacePatch {
        let surface = state.pane_surface().expect("surface installed");
        shepr_protocol::PaneSurfacePatch {
            boot_id: surface.boot_id.clone(),
            projection_revision: surface.projection_revision,
            base_surface_revision: surface.surface_revision,
            surface_revision: shepr_test_fixtures::counter_at(2),
            rows: Vec::new(),
            panes: Vec::new(),
            cursor,
        }
    }

    #[test]
    fn composed_cursor_suppression_blocks_cursor_only_patches() {
        let (mut state, area) = state_with_copy_pane_focus(false);
        let patch = cursor_patch(&state, Some(cursor(2)));
        assert!(!fast_path_blocker(&state, &patch, area));
        state.presentation.composition_mut().pane_cursor_overridden = true;
        assert!(fast_path_blocker(&state, &patch, area));
    }

    #[test]
    fn pane_patch_matching_is_limited_to_the_updated_pane_ids() {
        let updated = [
            crate::tests::test_pane_id("w1:p1"),
            crate::tests::test_pane_id("w1:p2"),
        ];
        let copy_pane = crate::tests::test_pane_id("w1:p1");
        let parked_copy_pane = crate::tests::test_pane_id("w2:p1");

        assert!(patch_updates_pane(updated.iter(), &copy_pane));
        assert!(!patch_updates_pane(updated.iter(), &parked_copy_pane));
    }

    #[test]
    fn cursor_only_copy_blocker_is_limited_to_a_changed_cursor_on_its_owner() {
        let (focused, area) = state_with_copy_pane_focus(true);
        let changed_cursor = cursor_patch(&focused, Some(cursor(2)));
        assert!(fast_path_blocker(&focused, &changed_cursor, area));

        let unchanged_cursor = cursor_patch(&focused, Some(cursor(1)));
        assert!(!fast_path_blocker(&focused, &unchanged_cursor, area));

        let (parked, area) = state_with_copy_pane_focus(false);
        let unrelated_cursor = cursor_patch(&parked, Some(cursor(2)));
        assert!(!fast_path_blocker(&parked, &unrelated_cursor, area));
    }
}
