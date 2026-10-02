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
        // unzooming always succeeds. The focus change is already committed, so
        // no path below may return None (which callers read as "pane not
        // found"); a refusal would degrade to an unchanged zoom instead.
        if !workspace.set_zoomed(desired) {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
            });
        }
        self.mark_session_dirty();
        Some(PaneZoomOutcome {
            changed: true,
            focus_changed,
        })
    }
}
