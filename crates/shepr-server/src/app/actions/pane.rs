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
        let tab_idx = self
            .workspaces
            .get(ws_idx)?
            .find_tab_index_for_pane(pane_id)?;
        let focus_changed = self.focus_pane_in_workspace(ws_idx, pane_id);
        let tab = self.workspaces.get(ws_idx)?.tabs().get(tab_idx)?;
        let zoomed = tab.zoomed();
        let desired = match command {
            PaneZoomCommand::Toggle => !zoomed,
            PaneZoomCommand::On => true,
            PaneZoomCommand::Off => false,
        };
        // A lone pane has nothing to zoom over, and a tab already in the
        // asked state stays as it is.
        if tab.layout().pane_count() <= 1 || desired == zoomed {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
            });
        }

        if !self
            .workspaces
            .get_mut(ws_idx)?
            .set_tab_zoomed(tab_idx, desired)
        {
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
    /// The focused tab of the session as (workspace index, tab index), with
    /// its tiled layout in the tab's layout area. Direction uses the tiled
    /// layout even when the tab is zoomed, as the API does.
    fn focused_tab_layout(&self) -> Option<(usize, usize, Vec<shepr_core::layout::PaneInfo>)> {
        let ws_idx = self.active_index()?;
        let workspace = self.workspaces.get(ws_idx)?;
        let tab_idx = workspace.active_tab_index();
        let area = shepr_mux::workspace::layout_rect(self.tab_layout_area(ws_idx, tab_idx));
        Some((ws_idx, tab_idx, workspace.active_tab().layout().panes(area)))
    }

    pub fn navigate_pane(&mut self, direction: NavDirection) {
        let Some((ws_idx, _, panes)) = self.focused_tab_layout() else {
            return;
        };
        if let Some(focused) = panes.iter().find(|p| p.is_focused)
            && let Some(target) = find_in_direction(focused, direction, &panes)
        {
            self.focus_pane_in_workspace(ws_idx, target);
        }
    }

    pub fn swap_pane(&mut self, direction: NavDirection) -> bool {
        let Some((ws_idx, tab_idx, panes)) = self.focused_tab_layout() else {
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
            .is_some_and(|workspace| workspace.swap_panes_in_tab(tab_idx, source, target));
        if changed {
            self.mark_session_dirty();
        }
        changed
    }

    pub fn resize_pane(&mut self, direction: NavDirection) {
        let Some(ws_idx) = self.active_index() else {
            return;
        };
        let Some(tab_idx) = self
            .workspaces
            .get(ws_idx)
            .map(shepr_mux::workspace::Workspace::active_tab_index)
        else {
            return;
        };
        let area = shepr_mux::workspace::layout_rect(self.tab_layout_area(ws_idx, tab_idx));
        let resized = self.workspaces.get_mut(ws_idx).is_some_and(|workspace| {
            workspace.resize_focused_pane_in_tab(tab_idx, direction, 0.05, area)
        });
        if resized {
            self.mark_session_dirty();
        }
    }
}
