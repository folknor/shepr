use super::*;

// ---------------------------------------------------------------------------
// Workspace operations
// ---------------------------------------------------------------------------

impl AppState {
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
        self.reconcile_bookmark();
        true
    }

    pub(crate) fn terminal_ids_for_workspace(
        &self,
        ws_idx: usize,
    ) -> Vec<shepr_protocol::TerminalId> {
        self.workspaces
            .get(ws_idx)
            .into_iter()
            .flat_map(|workspace| workspace.panes().values())
            .map(|pane| pane.attached_terminal_id.clone())
            .collect()
    }

    pub(crate) fn pane_ids_for_workspace(&self, ws_idx: usize) -> Vec<PaneId> {
        self.workspaces
            .get(ws_idx)
            .into_iter()
            .flat_map(|workspace| workspace.layout().pane_ids())
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
                ws.panes()
                    .values()
                    .any(|pane| pane.attached_terminal_id == terminal_id)
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
            .position(|workspace| workspace.contains_pane(pane_id))?;
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
                    scope: removal.scope,
                    pane_ids: closed.pane_ids,
                    terminal_ids: closed.terminal_ids,
                },
                detached_terminal_ids: closed.detached_terminal_ids,
            });
        }

        let detached_terminal_ids =
            self.remove_unattached_terminal_ids(removal.terminal_ids.iter().cloned());
        self.mark_session_dirty();
        PaneRemovalCommit::Removed(PaneRemovalOutcome {
            workspace_index: plan.workspace_index,
            removal,
            detached_terminal_ids,
        })
    }

    /// Closes the workspace at `ws_idx` and everything it owns. A bookmark on
    /// it moves to the workspace now at its index; every client location is
    /// settled by the server loop.
    pub(crate) fn close_workspace_at(&mut self, ws_idx: usize) -> Option<WorkspaceRemovalOutcome> {
        let workspace_id = self.workspaces.get(ws_idx).map(|ws| ws.id.clone())?;
        self.mark_session_dirty();
        crate::logging::workspace_closed(&workspace_id);

        let terminal_ids = self.terminal_ids_for_workspace(ws_idx);
        let pane_ids = self.pane_ids_for_workspace(ws_idx);

        self.workspaces.remove(ws_idx);
        let detached_terminal_ids =
            self.remove_unattached_terminal_ids(terminal_ids.iter().cloned());
        self.reconcile_bookmark();
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
}
