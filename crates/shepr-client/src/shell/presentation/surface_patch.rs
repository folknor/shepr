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
    // A cursor-only patch needs no copy-mode check of its own. A copy session replaces the
    // pane cursor only in Copy mode, and a frame composed in Copy mode records
    // `pane_cursor_overridden`, which blocks every patch above. Every way into Copy mode
    // (its binding, or a shown snapshot refocusing a parked session) leaves the client's
    // presentation dirty, so a patch arriving before that compose is never written as rows.
    // A parked session draws the pane's own cursor, which the patch carries.
    let copy_mode_patched = state.copy.as_ref().is_some_and(|copy_mode| {
        patch_updates_pane(
            patch.panes.iter().map(|pane| &pane.pane_id),
            &copy_mode.pane_id,
        )
    });
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
            // Copy mode is entered only on a pane that reports its scroll position.
            scroll: Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
                0,
                0,
                usize::from(rect.height),
                shepr_term::AbsRow(0),
            )),
            focused,
            mouse_reporting: false,
            pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
            alternate_screen_active: false,
        }
    }

    /// Presents a surface of `panes` over the whole pane area, with the cursor at (1, 0).
    fn present(
        state: &mut ClientShellState,
        area: Rect,
        surface_revision: shepr_protocol::SurfaceRevision,
        panes: Vec<shepr_protocol::PaneSurfacePane>,
    ) {
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
                surface_revision,
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
    }

    /// A shell that entered copy mode on its first pane, the only one when
    /// `copy_pane_focused`. Otherwise a second pane beside it takes focus afterwards,
    /// which parks the session.
    fn state_with_copy_pane_focus(copy_pane_focused: bool) -> (ClientShellState, Rect) {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        let mut snapshot = crate::shell::tests::snapshot();
        let copy_pane_id = snapshot.panes[0].pane_id;
        let other_pane_id = crate::tests::test_pane_id("w1:p2");
        if !copy_pane_focused {
            let mut other_pane = snapshot.panes[0].clone();
            other_pane.pane_id = other_pane_id;
            snapshot.panes.push(other_pane);
        }
        state.set_snapshot(Box::new(snapshot.clone()));
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
        let other_rect = shepr_protocol::SurfaceRect {
            x: copy_rect.width,
            y: 0,
            width: area.width.saturating_sub(copy_rect.width),
            height: area.height,
        };
        let panes = |copy_pane_has_focus: bool| {
            let mut panes = vec![test_surface_pane(
                copy_pane_id,
                copy_pane_has_focus,
                copy_rect,
            )];
            if !copy_pane_focused {
                panes.push(test_surface_pane(
                    other_pane_id,
                    !copy_pane_has_focus,
                    other_rect,
                ));
            }
            panes
        };
        present(
            &mut state,
            area,
            shepr_protocol::SurfaceRevision::FIRST,
            panes(true),
        );
        state.compose(80, 24).expect("terminal frame");
        assert!(state.enter_copy_mode(&mut crate::shell::state::ClientShellInput::default()));
        if !copy_pane_focused {
            snapshot.focused_pane_id = Some(other_pane_id);
            state.set_snapshot(Box::new(snapshot));
            present(
                &mut state,
                area,
                shepr_protocol::SurfaceRevision::FIRST
                    .checked_next()
                    .expect("test precondition"),
                panes(false),
            );
        }
        assert!(state.copy.is_some());
        assert_eq!(
            state.mode.is(crate::shell::state::ClientShellMode::Copy),
            copy_pane_focused
        );
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
            surface_revision: surface
                .surface_revision
                .checked_next()
                .expect("test precondition"),
            rows: Vec::new(),
            panes: Vec::new(),
            cursor,
        }
    }

    #[test]
    fn composed_cursor_suppression_blocks_cursor_only_patches() {
        // A parked session draws the pane's own cursor, so moving it takes the fast path.
        let (parked, area) = state_with_copy_pane_focus(false);
        let patch = cursor_patch(&parked, Some(cursor(2)));
        assert!(!fast_path_blocker(&parked, &patch, area));

        // A frame drawn in Copy mode replaces the pane cursor, so every cursor-only patch
        // composes, whether it moves the cursor or not.
        let (mut state, area) = state_with_copy_pane_focus(true);
        state.compose(80, 24).expect("copy-mode frame");
        assert!(state.presentation.composition().pane_cursor_overridden);
        let patch = cursor_patch(&state, Some(cursor(2)));
        assert!(fast_path_blocker(&state, &patch, area));
        let unchanged = cursor_patch(&state, Some(cursor(1)));
        assert!(fast_path_blocker(&state, &unchanged, area));
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
}
