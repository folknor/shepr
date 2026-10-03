use super::*;

// ---------------------------------------------------------------------------
// Pane operations
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn set_pane_input(
        &mut self,
        workspace_index: usize,
        pane_id: PaneId,
        right_click_passthrough: bool,
    ) -> Option<ViewMutation> {
        let pane = self
            .workspaces
            .get_mut(workspace_index)?
            .pane_state_mut(pane_id)?;
        if pane.right_click_passthrough == right_click_passthrough {
            return Some(ViewMutation::Unchanged);
        }
        pane.right_click_passthrough = right_click_passthrough;
        Some(ViewMutation::Metadata)
    }

    pub(crate) fn rename_terminal(
        &mut self,
        terminal_id: &shepr_protocol::TerminalId,
        label: Option<String>,
    ) -> Option<ViewMutation> {
        let terminal = self.terminals.get_mut(terminal_id)?;
        if terminal.manual_label() == label.as_deref() {
            return Some(ViewMutation::Unchanged);
        }
        match label {
            Some(label) => terminal.set_manual_label(label),
            None => terminal.clear_manual_label(),
        }
        self.mark_session_dirty();
        Some(ViewMutation::Metadata)
    }

    /// Applies a geometry edit and records its session consequence together.
    pub(crate) fn edit_workspace_geometry(
        &mut self,
        workspace_index: usize,
        edit: impl FnOnce(&mut shepr_mux::workspace::Workspace) -> bool,
    ) -> ViewMutation {
        if !self.workspaces.get_mut(workspace_index).is_some_and(edit) {
            return ViewMutation::Unchanged;
        }
        self.mark_session_dirty();
        ViewMutation::Geometry
    }

    pub(crate) fn swap_workspace_panes(
        &mut self,
        workspace_index: usize,
        source: PaneId,
        target: PaneId,
    ) -> ViewMutation {
        let Some(workspace) = self.workspaces.get_mut(workspace_index) else {
            return ViewMutation::Unchanged;
        };
        let focused = workspace.focused_pane_id();
        if !workspace.swap_panes(source, target) {
            return ViewMutation::Unchanged;
        }
        workspace.focus_pane(source);
        let focus_changed = workspace.focused_pane_id() != focused;
        self.mark_session_dirty();
        ViewMutation::Swap { focus_changed }
    }

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
        let focus_changed = self.focus_pane_in_workspace(ws_idx, pane_id).changed();
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
