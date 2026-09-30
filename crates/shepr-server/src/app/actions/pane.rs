use super::*;

// ---------------------------------------------------------------------------
// Pane operations
// ---------------------------------------------------------------------------

impl AppState {
    /// Toggles the zoom of workspace `ws_idx` on `pane_id`, focusing the pane
    /// first. `None` when the pane is not in the workspace. A workspace of one
    /// pane has nothing to zoom over, so its toggle changes nothing, though
    /// the pane is still focused.
    pub(crate) fn toggle_pane_zoom(
        &mut self,
        ws_idx: usize,
        pane_id: PaneId,
    ) -> Option<PaneZoomOutcome> {
        if !self.workspaces.get(ws_idx)?.contains_pane(pane_id) {
            return None;
        }
        let focus_changed = self.focus_pane_in_workspace(ws_idx, pane_id);
        let workspace = self.workspaces.get_mut(ws_idx)?;
        if workspace.pane_count() <= 1 {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
            });
        }

        let desired = !workspace.zoomed();
        // set_zoomed rejects only zooming a one-pane workspace. The count
        // check above already handles that case without reporting it missing;
        // unzooming always succeeds.
        workspace.set_zoomed(desired);
        if workspace.zoomed() != desired {
            return None;
        }
        self.mark_session_dirty();
        Some(PaneZoomOutcome {
            changed: true,
            focus_changed,
        })
    }
}

#[cfg(test)]
impl AppState {
    /// Workspace `ws_idx` as (its tiled layout in the workspace's layout
    /// area). Direction uses the tiled layout even when the workspace is
    /// zoomed, as the endpoint does.
    fn workspace_tiled_layout(&self, ws_idx: usize) -> Option<Vec<shepr_core::layout::PaneInfo>> {
        let workspace = self.workspaces.get(ws_idx)?;
        let area = shepr_mux::workspace::layout_rect(self.workspace_layout_area(ws_idx));
        Some(workspace.layout().panes(area))
    }

    pub fn navigate_pane(&mut self, ws_idx: usize, direction: NavDirection) {
        let Some(panes) = self.workspace_tiled_layout(ws_idx) else {
            return;
        };
        if let Some(focused) = panes.iter().find(|p| p.is_focused)
            && let Some(target) = find_in_direction(focused, direction, &panes)
        {
            self.focus_pane_in_workspace(ws_idx, target);
        }
    }

    pub fn swap_pane(&mut self, ws_idx: usize, direction: NavDirection) -> bool {
        let Some(panes) = self.workspace_tiled_layout(ws_idx) else {
            return false;
        };
        let Some(focused) = panes.iter().find(|p| p.is_focused) else {
            return false;
        };
        let Some(target) = find_in_direction(focused, direction, &panes) else {
            return false;
        };
        let source = focused.id;
        let changed = self
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|workspace| workspace.swap_panes(source, target));
        if changed {
            self.mark_session_dirty();
        }
        changed
    }

    pub fn resize_pane(&mut self, ws_idx: usize, direction: NavDirection) {
        let area = shepr_mux::workspace::layout_rect(self.workspace_layout_area(ws_idx));
        let resized = self
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|workspace| workspace.resize_focused_pane(direction, 0.05, area));
        if resized {
            self.mark_session_dirty();
        }
    }
}
