use super::*;

// ---------------------------------------------------------------------------
// Workspace operations
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn rename_workspace(
        &mut self,
        id: &shepr_protocol::WorkspaceId,
        name: String,
    ) -> Option<ViewMutation> {
        let workspace = self.workspaces.get_mut(id)?;
        if !workspace.set_name(name) {
            return Some(ViewMutation::Unchanged);
        }
        crate::logging::workspace_renamed(&workspace.id());
        self.mark_session_dirty();
        Some(ViewMutation::Metadata)
    }

    /// Moves workspace `id` to sit before `before`, or to the end when it is
    /// `None`. The bookmark follows its workspace.
    pub(crate) fn move_workspace(
        &mut self,
        id: &shepr_protocol::WorkspaceId,
        before: Option<&shepr_protocol::WorkspaceId>,
    ) -> ViewMutation {
        if !self.workspaces.move_before(id, before) {
            return ViewMutation::Unchanged;
        }
        self.mark_session_dirty();
        ViewMutation::WorkspaceOrder
    }

    /// Removes `pane_id` from the workspace that holds it, and the workspace
    /// with it when the pane was its last. `None` when no workspace holds the
    /// pane. The removal is one step: the set takes the pane (or the
    /// workspace) out, and the caller shuts down the runtimes the outcome
    /// names.
    pub(crate) fn remove_pane(&mut self, pane_id: PaneId) -> Option<PaneRemovalOutcome> {
        let removal = self.workspaces.remove_pane(pane_id)?;
        self.mark_session_dirty();
        if removal.scope == PaneRemovalScope::Workspace {
            crate::logging::workspace_closed(&removal.workspace_id);
        }
        let removed = self.forget_removed_panes(&removal.removed);
        Some(PaneRemovalOutcome {
            workspace_id: removal.workspace_id,
            pane_id: removal.pane,
            scope: removal.scope,
            focus_changed: removal.focus_changed,
            removed,
        })
    }

    /// Closes workspace `workspace_id` and everything it owns. A bookmark on
    /// it moves to the workspace now at its index; every client location is
    /// settled by the server loop. `None` when it is not a workspace.
    pub(crate) fn close_workspace(
        &mut self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<WorkspaceRemovalOutcome> {
        // The set repairs the bookmark: one on this workspace moves to the
        // workspace now at its index.
        let workspace = self.workspaces.remove(workspace_id)?;
        self.mark_session_dirty();
        crate::logging::workspace_closed(workspace_id);
        let removed: Vec<PaneId> = workspace.tree().panes().map(|(pane, _)| pane).collect();
        for pane in &removed {
            self.lifecycle_authority_dirty.remove(pane);
        }
        Some(WorkspaceRemovalOutcome {
            workspace_id: *workspace_id,
            removed,
        })
    }

    /// The panes that just left their workspace. Their pending authority syncs
    /// go too: a pane that is gone has no runtime to sync.
    fn forget_removed_panes(
        &mut self,
        removed: &[(PaneId, shepr_mux::workspace::PaneRecord)],
    ) -> Vec<PaneId> {
        removed
            .iter()
            .map(|(pane, _)| {
                self.lifecycle_authority_dirty.remove(pane);
                *pane
            })
            .collect()
    }
}
