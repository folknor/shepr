use super::*;

// ---------------------------------------------------------------------------
// Workspace operations
// ---------------------------------------------------------------------------

impl AppState {
    pub fn switch_workspace(&mut self, idx: usize) {
        if idx < self.workspaces.len() {
            let previous_focus = self.current_pane_focus_target();
            self.set_active_index(Some(idx));
            self.set_selected_index(Some(idx));
            let workspace_id = self.workspaces[idx].id.clone();
            crate::logging::workspace_focused(&workspace_id);
            self.mark_session_dirty();
            if let Some(ws) = self.workspaces.get_mut(idx) {
                let active_tab = ws.active_tab_index();
                ws.switch_tab(active_tab);
                let tab_id = public_tab_id_for_index(ws, active_tab);
                crate::logging::tab_focused(
                    &workspace_id,
                    tab_id.as_deref().unwrap_or(&workspace_id),
                );
            }
            self.record_pane_focus_after_navigation(previous_focus);
        }
    }

    pub(crate) fn switch_workspace_tab(&mut self, ws_idx: usize, tab_idx: usize) -> bool {
        if ws_idx >= self.workspaces.len() {
            return false;
        }
        if self
            .workspaces
            .get(ws_idx)
            .is_none_or(|ws| tab_idx >= ws.tabs().len())
        {
            return false;
        }

        let previous_focus = self.current_pane_focus_target();
        let workspace_changed = self.active_index() != Some(ws_idx);
        self.set_active_index(Some(ws_idx));
        self.set_selected_index(Some(ws_idx));
        let workspace_id = self.workspaces[ws_idx].id.clone();
        if workspace_changed {
            crate::logging::workspace_focused(&workspace_id);
        }
        self.mark_session_dirty();
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.switch_tab(tab_idx);
            let tab_id = public_tab_id_for_index(ws, tab_idx);
            crate::logging::tab_focused(&workspace_id, tab_id.as_deref().unwrap_or(&workspace_id));
        }
        self.refresh_active_tab_id();
        self.record_pane_focus_after_navigation(previous_focus);
        true
    }

    pub fn move_workspace(&mut self, source_idx: usize, insert_idx: usize) -> bool {
        if source_idx >= self.workspaces.len() || insert_idx > self.workspaces.len() {
            return false;
        }

        let target_idx = if source_idx < insert_idx {
            insert_idx - 1
        } else {
            insert_idx
        };
        if source_idx == target_idx {
            return false;
        }

        self.mark_session_dirty();

        let workspace = self.workspaces.remove(source_idx);
        self.workspaces.insert(target_idx, workspace);
        true
    }

    pub(crate) fn terminal_ids_for_workspace(
        &self,
        ws_idx: usize,
    ) -> Vec<shepr_protocol::TerminalId> {
        self.workspaces
            .get(ws_idx)
            .into_iter()
            .flat_map(shepr_mux::workspace::Workspace::tabs)
            .flat_map(|tab| tab.panes().values())
            .map(|pane| pane.attached_terminal_id.clone())
            .collect()
    }

    pub(crate) fn pane_ids_for_workspace(&self, ws_idx: usize) -> Vec<PaneId> {
        self.workspaces
            .get(ws_idx)
            .into_iter()
            .flat_map(shepr_mux::workspace::Workspace::tabs)
            .flat_map(|tab| tab.layout().pane_ids())
            .collect()
    }

    /// Drops the metadata of every listed terminal no pane still attaches and
    /// returns those terminals, whose runtimes the caller must shut down.
    #[must_use = "the detached terminals' runtimes must be shut down"]
    pub(crate) fn remove_unattached_terminal_ids(
        &mut self,
        terminal_ids: impl IntoIterator<Item = shepr_protocol::TerminalId>,
    ) -> Vec<shepr_protocol::TerminalId> {
        let mut detached = Vec::new();
        for terminal_id in terminal_ids {
            let still_attached = self.workspaces.iter().any(|ws| {
                ws.tabs().iter().any(|tab| {
                    tab.panes()
                        .values()
                        .any(|pane| pane.attached_terminal_id == terminal_id)
                })
            });
            if still_attached {
                continue;
            }
            if self.terminals.remove(&terminal_id).is_some() {
                detached.push(terminal_id);
            }
        }
        detached
    }

    pub(crate) fn prepare_pane_removal(
        &self,
        workspace_index: usize,
        pane_id: PaneId,
    ) -> Option<PaneRemovalPlan> {
        let workspace_plan = self
            .workspaces
            .get(workspace_index)?
            .prepare_pane_removal(pane_id)?;
        Some(PaneRemovalPlan {
            workspace_index,
            workspace_plan,
        })
    }

    pub(crate) fn prepare_pane_removal_by_id(&self, pane_id: PaneId) -> Option<PaneRemovalPlan> {
        let workspace_index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.find_tab_index_for_pane(pane_id).is_some())?;
        self.prepare_pane_removal(workspace_index, pane_id)
    }

    pub(crate) fn commit_pane_removal(&mut self, plan: &PaneRemovalPlan) -> PaneRemovalCommit {
        let Some(workspace) = self.workspaces.get_mut(plan.workspace_index) else {
            return PaneRemovalCommit::Stale;
        };
        let Some(removal) = workspace.remove_pane(&plan.workspace_plan) else {
            return PaneRemovalCommit::Stale;
        };

        if removal.scope == PaneRemovalScope::Workspace {
            let Some(closed) = self.close_workspace_at(plan.workspace_index) else {
                return PaneRemovalCommit::Stale;
            };
            return PaneRemovalCommit::Removed(PaneRemovalOutcome {
                workspace_index: plan.workspace_index,
                removal: PaneRemoval {
                    workspace_id: closed.workspace_id,
                    pane_id: removal.pane_id,
                    tab_index: removal.tab_index,
                    scope: removal.scope,
                    pane_ids: closed.pane_ids,
                    terminal_ids: closed.terminal_ids,
                },
                detached_terminal_ids: closed.detached_terminal_ids,
            });
        }

        self.clear_stale_previous_pane_focus(removal.pane_ids.iter().copied());
        let detached_terminal_ids =
            self.remove_unattached_terminal_ids(removal.terminal_ids.iter().cloned());
        if self.active_index() == Some(plan.workspace_index) {
            self.refresh_active_tab_id();
        }
        self.mark_session_dirty();
        PaneRemovalCommit::Removed(PaneRemovalOutcome {
            workspace_index: plan.workspace_index,
            removal,
            detached_terminal_ids,
        })
    }

    pub(crate) fn prepare_tab_removal(
        &self,
        workspace_index: usize,
        tab_index: usize,
    ) -> Option<TabRemovalPlan> {
        let workspace = self.workspaces.get(workspace_index)?;
        let tab_number = workspace.tabs().get(tab_index)?.number();
        Some(TabRemovalPlan {
            workspace_index,
            tab_index,
            scope: if workspace.tabs().len() <= 1 {
                TabRemovalScope::Workspace
            } else {
                TabRemovalScope::Tab
            },
            workspace_id: workspace.id.clone(),
            tab_number,
        })
    }

    pub(crate) fn commit_tab_removal(&mut self, plan: &TabRemovalPlan) -> TabRemovalCommit {
        let Some(workspace) = self.workspaces.get(plan.workspace_index) else {
            return TabRemovalCommit::Stale;
        };
        let Some(tab) = workspace.tabs().get(plan.tab_index) else {
            return TabRemovalCommit::Stale;
        };
        let expected_scope = if workspace.tabs().len() <= 1 {
            TabRemovalScope::Workspace
        } else {
            TabRemovalScope::Tab
        };
        if workspace.id != plan.workspace_id
            || tab.number() != plan.tab_number
            || expected_scope != plan.scope
        {
            return TabRemovalCommit::Stale;
        }
        if plan.scope == TabRemovalScope::Workspace {
            let Some(closed) = self.close_workspace_at(plan.workspace_index) else {
                return TabRemovalCommit::Stale;
            };
            return TabRemovalCommit::Removed(TabRemovalOutcome {
                workspace_index: plan.workspace_index,
                scope: plan.scope,
                pane_ids: closed.pane_ids,
                terminal_ids: closed.terminal_ids,
                detached_terminal_ids: closed.detached_terminal_ids,
                tab: None,
            });
        }

        let Some(removal) = self
            .workspaces
            .get_mut(plan.workspace_index)
            .and_then(|workspace| workspace.close_tab(plan.tab_index))
        else {
            return TabRemovalCommit::Stale;
        };
        if self.active_index() == Some(plan.workspace_index) {
            self.refresh_active_tab_id();
        }
        self.clear_stale_previous_pane_focus(removal.pane_ids.iter().copied());
        let detached_terminal_ids =
            self.remove_unattached_terminal_ids(removal.terminal_ids.iter().cloned());
        self.mark_session_dirty();
        let tab_id = shepr_protocol::PublicTabId::new(&removal.workspace_id, removal.tab_number);
        crate::logging::tab_closed(&removal.workspace_id, &tab_id);
        TabRemovalCommit::Removed(TabRemovalOutcome {
            workspace_index: plan.workspace_index,
            scope: plan.scope,
            pane_ids: removal.pane_ids.clone(),
            terminal_ids: removal.terminal_ids.clone(),
            detached_terminal_ids,
            tab: Some(removal),
        })
    }

    pub(crate) fn clear_stale_previous_pane_focus(
        &mut self,
        pane_ids: impl IntoIterator<Item = PaneId>,
    ) {
        let pane_ids = pane_ids.into_iter().collect::<Vec<_>>();
        if self
            .previous_pane_focus
            .as_ref()
            .is_some_and(|focus| pane_ids.contains(&focus.pane_id))
        {
            self.previous_pane_focus = None;
        }
    }

    /// Closes the workspace at `ws_idx` and everything it owns.
    ///
    /// Focus stays on the previously active workspace and the sidebar cursor
    /// on the previously selected one when they survive; otherwise both fall
    /// back to the closed slot (clamped). Closing the last workspace leaves
    /// terminal mode, since there is no pane left to type into.
    pub(crate) fn close_workspace_at(&mut self, ws_idx: usize) -> Option<WorkspaceRemovalOutcome> {
        let workspace_id = self.workspaces.get(ws_idx).map(|ws| ws.id.clone())?;
        self.mark_session_dirty();
        crate::logging::workspace_closed(&workspace_id);

        let terminal_ids = self.terminal_ids_for_workspace(ws_idx);
        let pane_ids = self.pane_ids_for_workspace(ws_idx);
        let active_workspace_id = self.active.clone();
        let selected_workspace_id = self.selected.clone();

        self.clear_stale_previous_pane_focus(pane_ids.iter().copied());
        self.workspaces.remove(ws_idx);
        let detached_terminal_ids =
            self.remove_unattached_terminal_ids(terminal_ids.iter().cloned());

        if self.workspaces.is_empty() {
            self.set_active_index(None);
            self.set_selected_index(None);
            if self.mode == Mode::Terminal {
                self.mode = Mode::Navigate;
            }
            return Some(WorkspaceRemovalOutcome {
                workspace_id,
                pane_ids,
                terminal_ids,
                detached_terminal_ids,
            });
        }
        let last = self.workspaces.len() - 1;
        let position_of =
            |id: Option<shepr_protocol::WorkspaceId>,
             workspaces: &[shepr_mux::workspace::Workspace]| {
                id.and_then(|id| workspaces.iter().position(|ws| ws.id == id))
            };
        let active = position_of(active_workspace_id, &self.workspaces).unwrap_or(ws_idx.min(last));
        self.set_active_index(Some(active));
        self.set_selected_index(Some(
            position_of(selected_workspace_id, &self.workspaces).unwrap_or(active),
        ));
        Some(WorkspaceRemovalOutcome {
            workspace_id,
            pane_ids,
            terminal_ids,
            detached_terminal_ids,
        })
    }
}

#[cfg(test)]
impl AppState {
    pub fn switch_tab(&mut self, idx: usize) {
        if let Some(ws_idx) = self.active_index() {
            let previous_focus = self.current_pane_focus_target();
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return;
            };
            ws.switch_tab(idx);
            let tab_id = public_tab_id_for_index(ws, idx);
            crate::logging::tab_focused(&ws.id, tab_id.as_deref().unwrap_or(&ws.id));
            self.refresh_active_tab_id();
            self.mark_session_dirty();
            self.record_pane_focus_after_navigation(previous_focus);
        }
    }

    pub(crate) fn terminal_id_for_pane(
        &self,
        ws_idx: usize,
        pane_id: PaneId,
    ) -> Option<shepr_protocol::TerminalId> {
        self.workspaces
            .get(ws_idx)?
            .pane_state(pane_id)
            .map(|pane| pane.attached_terminal_id.clone())
    }

    pub(crate) fn remove_pane(
        &mut self,
        workspace_index: usize,
        pane_id: PaneId,
    ) -> PaneRemovalCommit {
        let Some(plan) = self.prepare_pane_removal(workspace_index, pane_id) else {
            return PaneRemovalCommit::Stale;
        };
        self.commit_pane_removal(&plan)
    }

    pub(crate) fn remove_active_tab(&mut self) -> TabRemovalCommit {
        let Some(workspace_index) = self.active_index() else {
            return TabRemovalCommit::Stale;
        };
        let Some(tab_index) = self
            .workspaces
            .get(workspace_index)
            .map(shepr_mux::workspace::Workspace::active_tab_index)
        else {
            return TabRemovalCommit::Stale;
        };
        let Some(plan) = self.prepare_tab_removal(workspace_index, tab_index) else {
            return TabRemovalCommit::Stale;
        };
        self.commit_tab_removal(&plan)
    }

    pub fn close_selected_workspace(&mut self) {
        self.close_workspace_at(self.selected_index().unwrap_or(0));
    }
}
