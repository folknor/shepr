use super::*;

// ---------------------------------------------------------------------------
// Pane operations
// ---------------------------------------------------------------------------

impl AppState {
    #[cfg(test)]
    pub fn navigate_pane(&mut self, direction: NavDirection) {
        let Some(ws_idx) = self.active_index() else {
            return;
        };
        let Some(tab) = self
            .workspaces
            .get(ws_idx)
            .map(shepr_mux::workspace::Workspace::active_tab)
        else {
            return;
        };
        let panes = if tab.zoomed() {
            tab.layout()
                .panes(shepr_mux::workspace::layout_rect(self.view.terminal_area))
        } else {
            self.view
                .pane_infos
                .iter()
                .cloned()
                .map(Into::into)
                .collect()
        };

        if let Some(focused) = panes.iter().find(|p| p.is_focused)
            && let Some(target) = find_in_direction(focused, direction, &panes)
        {
            self.focus_pane_in_workspace(ws_idx, target);
        }
    }

    #[cfg(test)]
    pub fn swap_pane(&mut self, direction: NavDirection) -> bool {
        let Some(ws_idx) = self.active_index() else {
            return false;
        };
        let Some(tab) = self
            .workspaces
            .get(ws_idx)
            .map(shepr_mux::workspace::Workspace::active_tab)
        else {
            return false;
        };
        let panes = if tab.zoomed() {
            tab.layout()
                .panes(shepr_mux::workspace::layout_rect(self.view.terminal_area))
        } else {
            self.view
                .pane_infos
                .iter()
                .cloned()
                .map(Into::into)
                .collect()
        };

        let Some(focused) = panes.iter().find(|p| p.is_focused) else {
            return false;
        };
        let Some(target) = find_in_direction(focused, direction, &panes) else {
            return false;
        };
        let source = focused.id;
        let Some(tab_idx) = self
            .workspaces
            .get(ws_idx)
            .map(shepr_mux::workspace::Workspace::active_tab_index)
        else {
            return false;
        };
        let changed = self
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|workspace| workspace.swap_panes_in_tab(tab_idx, source, target));
        if changed {
            self.mark_session_dirty();
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn resize_pane(&mut self, direction: NavDirection) {
        if let Some(first) = self.view.pane_infos.first() {
            let area = self
                .view
                .pane_infos
                .iter()
                .fold(first.rect, |acc, p| acc.union(p.rect));
            if let Some(workspace_index) = self.active_index() {
                let resized = self
                    .workspaces
                    .get_mut(workspace_index)
                    .is_some_and(|workspace| {
                        let tab_index = workspace.active_tab_index();
                        workspace.resize_focused_pane_in_tab(
                            tab_index,
                            direction,
                            0.05,
                            shepr_mux::workspace::layout_rect(area),
                        )
                    });
                if resized {
                    self.mark_session_dirty();
                }
            }
        }
    }

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
        let pane_count = tab.layout().pane_count();
        let zoomed = tab.zoomed();
        if pane_count <= 1 {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
                reason: Some(PaneZoomNoopReason::SinglePane),
                zoomed,
            });
        }

        let desired = match command {
            PaneZoomCommand::Toggle => !zoomed,
            PaneZoomCommand::On => true,
            PaneZoomCommand::Off => false,
        };
        let reason = match (command, zoomed) {
            (PaneZoomCommand::On, true) => Some(PaneZoomNoopReason::AlreadyZoomed),
            (PaneZoomCommand::Off, false) => Some(PaneZoomNoopReason::AlreadyUnzoomed),
            _ => None,
        };
        if reason.is_some() {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
                reason,
                zoomed,
            });
        }

        if !self
            .workspaces
            .get_mut(ws_idx)?
            .set_tab_zoomed(tab_idx, desired)
        {
            return None;
        }
        let zoomed = desired;
        self.mark_session_dirty();
        Some(PaneZoomOutcome {
            changed: true,
            focus_changed,
            reason: None,
            zoomed,
        })
    }
}
