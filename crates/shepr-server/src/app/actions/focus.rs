use super::*;

// ---------------------------------------------------------------------------
// Creation and pane focus
// ---------------------------------------------------------------------------

impl AppState {
    /// Adds a freshly spawned workspace at the end and records the geometry
    /// its first pane was spawned at. No client is moved onto it: navigation is
    /// per client and the server loop applies it. `None`, with nothing added,
    /// when the set refuses the workspace (a repeated workspace or pane ID,
    /// which a freshly prepared workspace cannot have).
    pub(crate) fn commit_workspace_creation(
        &mut self,
        prepared: shepr_mux::workspace::PreparedWorkspace,
        geometry: shepr_mux::workspace::SpawnGeometry,
    ) -> Option<WorkspaceCreationOutcome> {
        let root_pane = prepared.root_pane();
        let workspace_id = match self.workspaces.commit_workspace(prepared, geometry) {
            Ok(id) => id,
            Err(refused) => {
                tracing::error!(workspace = %refused.id(), "refused to add a new workspace");
                return None;
            }
        };
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
        Some(WorkspaceCreationOutcome {
            workspace_id,
            root_pane,
        })
    }

    /// Installs a spawned split as the focused pane of the workspace the
    /// token names. `None`, with nothing added, when the workspace refuses it.
    pub(crate) fn commit_pane_split(
        &mut self,
        split: shepr_mux::workspace::PreparedSplit,
    ) -> Option<PaneCreationOutcome> {
        let workspace_id = split.workspace_id();
        let workspace = self.workspaces.get_mut(&workspace_id)?;
        let pane_id = match workspace.commit_split(split) {
            Ok(pane_id) => pane_id,
            Err(refused) => {
                tracing::warn!(
                    workspace = %workspace_id,
                    ?refused,
                    "a spawned split was refused by its workspace"
                );
                return None;
            }
        };
        self.mark_session_dirty();
        self.mark_shell_projection_dirty();
        Some(PaneCreationOutcome {
            workspace_id,
            pane_id,
        })
    }

    /// Focuses `pane_id` in the workspace that holds it. Pane focus is shared
    /// by every client that views the workspace. Unchanged when no workspace
    /// holds the pane or it already has focus.
    pub(crate) fn focus_pane(&mut self, pane_id: PaneId) -> ViewMutation {
        let Some(ws) = self.workspace_of_mut(pane_id) else {
            return ViewMutation::Unchanged;
        };
        if ws.tree().focused() == pane_id {
            return ViewMutation::Unchanged;
        }
        if ws.focus_pane(pane_id) {
            self.mark_session_dirty();
            self.mark_shell_projection_dirty();
            return ViewMutation::Focus;
        }
        ViewMutation::Unchanged
    }
}
