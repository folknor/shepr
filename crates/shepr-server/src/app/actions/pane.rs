use super::*;

// ---------------------------------------------------------------------------
// Pane operations
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn set_pane_input(
        &mut self,
        pane_id: PaneId,
        right_click_passthrough: bool,
    ) -> Option<ViewMutation> {
        let pane = self.workspaces.pane_mut(pane_id)?;
        if !pane.set_right_click_passthrough(right_click_passthrough) {
            return Some(ViewMutation::Unchanged);
        }
        self.mark_shell_projection_dirty();
        Some(ViewMutation::Metadata)
    }

    /// Sets or clears the pane's manual label. `None` when no workspace holds
    /// the pane.
    pub(crate) fn rename_pane(
        &mut self,
        pane_id: PaneId,
        label: Option<shepr_mux::terminal::Label>,
    ) -> Option<ViewMutation> {
        let terminal = self.workspaces.pane_mut(pane_id)?.terminal_mut();
        if terminal.manual_label_value() == label.as_ref() {
            return Some(ViewMutation::Unchanged);
        }
        match label {
            Some(label) => terminal.set_manual_label(label),
            None => terminal.clear_manual_label(),
        }
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
        Some(ViewMutation::Metadata)
    }

    /// Applies a geometry edit to workspace `id` and records its session
    /// consequence together.
    pub(crate) fn edit_workspace_geometry(
        &mut self,
        id: &shepr_protocol::WorkspaceId,
        edit: impl FnOnce(&mut shepr_mux::workspace::Workspace) -> bool,
    ) -> ViewMutation {
        if !self.workspaces.get_mut(id).is_some_and(edit) {
            return ViewMutation::Unchanged;
        }
        self.mark_session_dirty();
        ViewMutation::Geometry
    }

    /// Swaps two panes of the workspace that holds `source` and focuses
    /// `source`. Unchanged when no workspace holds it or `target` is not in
    /// the same workspace.
    pub(crate) fn swap_panes(&mut self, source: PaneId, target: PaneId) -> ViewMutation {
        let Some(source_public) = self.workspaces.pane(source).map(|pane| pane.public_id()) else {
            return ViewMutation::Unchanged;
        };
        let Some(target_public) = self.workspaces.pane(target).map(|pane| pane.public_id()) else {
            return ViewMutation::Unchanged;
        };
        let Some(workspace) = self.workspace_of_mut(source) else {
            return ViewMutation::Unchanged;
        };
        let focused = workspace.tree().focused();
        if !workspace.swap_panes(source, target) {
            return ViewMutation::Unchanged;
        }
        workspace.focus_pane(source);
        let focus_changed = workspace.tree().focused() != focused;
        crate::logging::panes_swapped(source_public, target_public);
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
        ViewMutation::Swap { focus_changed }
    }

    /// Toggles the zoom of the workspace that holds `pane_id` on it, focusing
    /// the pane first. `None` when no workspace holds the pane. A workspace of
    /// one pane has nothing to zoom over, so its toggle does nothing at all:
    /// no zoom, no focus, nothing changed (the client offers no zoom there).
    pub(crate) fn toggle_pane_zoom(&mut self, pane_id: PaneId) -> Option<PaneZoomOutcome> {
        if self.workspaces.pane(pane_id)?.workspace().tree().is_lone() {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed: false,
            });
        }
        let public_id = self.workspaces.pane(pane_id)?.public_id();
        let focus_changed = self.focus_pane(pane_id).changed();
        let workspace = self.workspace_of_mut(pane_id)?;
        let desired = !workspace.tree().zoomed();
        // The lone-pane case was handled above. The focus change is already
        // committed, so no path below may return None (which callers read as
        // "pane not found"); a refusal would degrade to an unchanged zoom.
        if !workspace.set_zoomed(desired) {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
            });
        }
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
        crate::logging::pane_zoomed(public_id, desired);
        Some(PaneZoomOutcome {
            changed: true,
            focus_changed,
        })
    }
}
