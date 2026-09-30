use super::*;

// ---------------------------------------------------------------------------
// Pane operations
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn apply_pane_zoom(
        &mut self,
        ws_idx: usize,
        pane_id: PaneId,
        command: PaneZoomCommand,
    ) -> Option<PaneZoomOutcome> {
        if !self.workspaces.get(ws_idx)?.contains_pane(pane_id) {
            return None;
        }
        let focus_changed = self.focus_pane_in_workspace(ws_idx, pane_id);
        let workspace = self.workspaces.get_mut(ws_idx)?;
        let zoomed = workspace.zoomed();
        let desired = match command {
            PaneZoomCommand::Toggle => !zoomed,
            PaneZoomCommand::On => true,
            PaneZoomCommand::Off => false,
        };
        // Zoom needs two panes: a lone pane has nothing to zoom over, and a
        // workspace already in the asked state stays as it is.
        if workspace.pane_count() <= 1 || desired == zoomed {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
            });
        }

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
    /// The focused workspace of the session as (workspace index, tiled
    /// layout in the workspace's layout area). Direction uses the tiled
    /// layout even when the workspace is zoomed, as the API does.
    fn focused_workspace_layout(&self) -> Option<(usize, Vec<shepr_core::layout::PaneInfo>)> {
        let ws_idx = self.active_index()?;
        let workspace = self.workspaces.get(ws_idx)?;
        let area = shepr_mux::workspace::layout_rect(self.workspace_layout_area(ws_idx));
        Some((ws_idx, workspace.layout().panes(area)))
    }

    pub fn navigate_pane(&mut self, direction: NavDirection) {
        let Some((ws_idx, panes)) = self.focused_workspace_layout() else {
            return;
        };
        if let Some(focused) = panes.iter().find(|p| p.is_focused)
            && let Some(target) = find_in_direction(focused, direction, &panes)
        {
            self.focus_pane_in_workspace(ws_idx, target);
        }
    }

    pub fn swap_pane(&mut self, direction: NavDirection) -> bool {
        let Some((ws_idx, panes)) = self.focused_workspace_layout() else {
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

    pub fn resize_pane(&mut self, direction: NavDirection) {
        let Some(ws_idx) = self.active_index() else {
            return;
        };
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
