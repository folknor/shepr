use super::*;

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
    let Some(copy_mode) = state.copy_mode.as_ref() else {
        return false;
    };
    let Some(surface) = state.pane_surface() else {
        return false;
    };
    patch.cursor != surface.frame.cursor
        && surface
            .panes
            .iter()
            .any(|pane| pane.focused && pane.pane_id == copy_mode.pane_id)
}

fn fast_path_blocker(
    state: &ClientShellState,
    patch: &shepr_protocol::PaneSurfacePatch,
    area: Rect,
) -> Option<&'static str> {
    // Selection and copy mode affect pane cells only when their owner is patched. A parked
    // copy session must not send unrelated pane output through full-frame composition.
    if state
        .pane_surface()
        .is_some_and(|surface| super::composition::surface_overflows_area(surface, area))
    {
        // A surface produced for a larger pane area (before a resize or sidebar toggle took
        // effect) is drawn clipped by `compose`. Its patch rows, offset into this layout, could
        // land on the mode bar or past the frame, so they go through compose too.
        Some("client_surface_patch.fallback.geometry")
    } else if state.mode != ClientShellMode::Terminal {
        Some("client_surface_patch.fallback.mode")
    } else if state.overlay.is_some() {
        Some("client_surface_patch.fallback.overlay")
    } else if state.endpoint_error.is_some() {
        Some("client_surface_patch.fallback.endpoint_error")
    } else if state.visible_endpoint_notice.is_some() {
        // Notices are drawn over the panes; while one is up, pane updates go through a full
        // compose. Notices expire (see `tick_transient_banners`), so this only costs for as
        // long as one is on screen.
        Some("client_surface_patch.fallback.endpoint_notice")
    } else if state.selection.as_ref().is_some_and(|selection| {
        selection.is_visible()
            && patch_updates_pane(
                patch.panes.iter().map(|pane| &pane.pane_id),
                &selection.pane_id,
            )
    }) {
        Some("client_surface_patch.fallback.selection")
    } else if state.copy_mode.as_ref().is_some_and(|copy_mode| {
        patch_updates_pane(
            patch.panes.iter().map(|pane| &pane.pane_id),
            &copy_mode.pane_id,
        )
    }) || copy_mode_cursor_changed_on_owner(state, patch)
    {
        // The cursor is sampled independently of the changed pane list, so a patch can move it
        // without naming its owner in metadata. Recompose only when that owner has copy state.
        Some("client_surface_patch.fallback.copy_mode")
    } else if patch.panes.iter().any(|pane| {
        !state
            .hits
            .panes
            .iter()
            .any(|hit| hit.pane_id == pane.pane_id)
    }) {
        Some("client_surface_patch.fallback.pane_hits")
    } else {
        None
    }
}

impl ClientShellState {
    /// A patch from the shown connection `generation`; it must follow that
    /// connection's own baseline.
    pub(crate) fn apply_pane_surface_patch_from(
        &mut self,
        patch: &shepr_protocol::PaneSurfacePatch,
        generation: u64,
    ) -> ClientPaneSurfacePatchOutcome {
        self.apply_tagged_pane_surface_patch(patch, Some(generation))
    }

    pub(super) fn apply_tagged_pane_surface_patch(
        &mut self,
        patch: &shepr_protocol::PaneSurfacePatch,
        generation: surfaces::SurfaceGeneration,
    ) -> ClientPaneSurfacePatchOutcome {
        if let Err(reason) = self.surfaces.validate(patch, generation) {
            return ClientPaneSurfacePatchOutcome::Rejected(reason);
        }
        if !self.surfaces.is_paired() {
            return match self.surfaces.apply_validated(patch) {
                Ok(()) => ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held),
                Err(reason) => ClientPaneSurfacePatchOutcome::Rejected(reason),
            };
        }
        let (cols, rows) = self.last_composed_size.unwrap_or_default();
        let area = self.layout(cols, rows).pane_surface;
        let fast_path_blocker = fast_path_blocker(self, patch, area);
        let fast_path_area = fast_path_blocker.is_none().then_some(area);
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
            if let Err(reason) = self.surfaces.apply_validated(patch) {
                return ClientPaneSurfacePatchOutcome::Rejected(reason);
            }
            for updated in &patch.panes {
                let Some(hit) = self
                    .hits
                    .panes
                    .iter_mut()
                    .find(|hit| hit.pane_id == updated.pane_id)
                else {
                    continue;
                };
                hit.scrollbar_rect = updated.scrollbar_rect.map(|rect| {
                    Rect::new(
                        area.x.saturating_add(rect.x),
                        area.y.saturating_add(rect.y),
                        rect.width,
                        rect.height,
                    )
                });
                hit.scroll = updated.scroll.map(|metrics| shepr_termio::ScrollMetrics {
                    offset_from_bottom: usize::try_from(metrics.offset_from_bottom)
                        .unwrap_or(usize::MAX),
                    max_offset_from_bottom: usize::try_from(metrics.max_offset_from_bottom)
                        .unwrap_or(usize::MAX),
                    viewport_rows: usize::try_from(metrics.viewport_rows).unwrap_or(usize::MAX),
                    history_origin: metrics.history_origin,
                });
                hit.mouse_reporting = updated.mouse_reporting;
                hit.sgr_pixel_mouse = updated.sgr_pixel_mouse;
                hit.pixel_width = updated.pixel_width;
                hit.pixel_height = updated.pixel_height;
                self.scroll_target_shown(&updated.pane_id, updated.scroll);
            }
        } else {
            let before = self.pane_facts_before(self.surfaces.paired());
            if let Err(reason) = self.surfaces.apply_validated(patch) {
                return ClientPaneSurfacePatchOutcome::Rejected(reason);
            }
            let surfaces = std::mem::take(&mut self.surfaces);
            if let Some(surface) = surfaces.paired() {
                self.presented_surface_changed(before, surface);
            }
            self.surfaces = surfaces;
        }
        ClientPaneSurfacePatchOutcome::Applied(match composed_patch {
            Some(c) => PatchPresentation::Rows(c),
            None => PatchPresentation::Compose,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            content_revision: 0,
            rect,
            inner_rect: rect,
            scrollbar_rect: None,
            scroll: None,
            focused,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn state_with_copy_pane_focus(copy_pane_focused: bool) -> (ClientShellState, Rect) {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.last_composed_size = Some((80, 24));
        let mut snapshot = crate::shell::tests::snapshot();
        let copy_pane_id = snapshot.panes[0].pane_id.clone();
        let other_pane_id = crate::tests::test_pane_id("w1:p2");
        if !copy_pane_focused {
            snapshot.focused_pane_id = Some(other_pane_id.clone());
            snapshot.panes[0].focused = false;
            let mut other_pane = snapshot.panes[0].clone();
            other_pane.pane_id = other_pane_id.clone();
            other_pane.focused = true;
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
            vec![test_surface_pane(copy_pane_id.clone(), true, copy_rect)]
        } else {
            let other_rect = shepr_protocol::SurfaceRect {
                x: copy_rect.width,
                y: 0,
                width: area.width.saturating_sub(copy_rect.width),
                height: area.height,
            };
            vec![
                test_surface_pane(copy_pane_id.clone(), false, copy_rect),
                test_surface_pane(other_pane_id, true, other_rect),
            ]
        };
        let snapshot = state.snapshot.as_deref().expect("snapshot installed");
        let buffer = Buffer::empty(Rect::new(0, 0, area.width, area.height));
        state.receive_pane_surface(shepr_protocol::PaneSurfaceFrame {
            boot_id: snapshot.boot_id.clone(),
            projection_revision: snapshot.revision,
            surface_revision: shepr_protocol::SurfaceRevision::new(1),
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, Some(cursor(1)), &[]),
            panes,
            splits: Vec::new(),
        });
        state.copy_mode = Some(ClientCopyModeState {
            pane_id: copy_pane_id,
            history_origin: shepr_vt::AbsRow(0),
            geometry: (area.width, area.height),
            alternate_screen_active: false,
            cursor: shepr_protocol::command::PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            entry_offset_from_bottom: 0,
            selection: None,
            search_prompt: None,
            search_query: TypedText::default(),
            search_direction: None,
            search_matches: Vec::new(),
            search_total: 0,
            search_current: None,
            search_current_global: None,
            search_generation: 0,
            copy_after_search: false,
        });
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
            surface_revision: shepr_protocol::SurfaceRevision::new(2),
            rows: Vec::new(),
            panes: Vec::new(),
            cursor,
        }
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
        assert_eq!(
            fast_path_blocker(&focused, &changed_cursor, area),
            Some("client_surface_patch.fallback.copy_mode")
        );

        let unchanged_cursor = cursor_patch(&focused, Some(cursor(1)));
        assert_eq!(fast_path_blocker(&focused, &unchanged_cursor, area), None);

        let (parked, area) = state_with_copy_pane_focus(false);
        let unrelated_cursor = cursor_patch(&parked, Some(cursor(2)));
        assert_eq!(fast_path_blocker(&parked, &unrelated_cursor, area), None);
    }
}
