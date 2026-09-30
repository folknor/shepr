use super::*;

// ---------------------------------------------------------------------------
// Creation and pane focus
// ---------------------------------------------------------------------------

impl AppState {
    /// Adds a freshly spawned workspace at the end. No client is moved onto
    /// it: navigation is per client and the server loop applies it.
    pub(crate) fn commit_workspace_creation(
        &mut self,
        workspace: shepr_mux::workspace::Workspace,
        terminal: shepr_mux::terminal::TerminalState,
    ) -> WorkspaceCreationOutcome {
        let workspace_id = workspace.id.clone();
        let root_pane = workspace.root_pane();
        self.terminals.insert(terminal.id.clone(), terminal);
        self.workspaces.push(workspace);
        let workspace_index = self.workspaces.len() - 1;
        self.mark_session_dirty();
        WorkspaceCreationOutcome {
            workspace_index,
            workspace_id,
            root_pane,
        }
    }

    /// Installs a spawned split as the workspace's focused pane.
    pub(crate) fn commit_pane_split(
        &mut self,
        workspace_index: usize,
        pane_id: PaneId,
        prepared_layout: shepr_core::layout::TileLayout,
        terminal: shepr_mux::terminal::TerminalState,
    ) -> Option<PaneCreationOutcome> {
        let terminal_id = terminal.id.clone();
        self.workspaces.get_mut(workspace_index)?.commit_new_pane(
            pane_id,
            prepared_layout,
            terminal_id.clone(),
            true,
        )?;
        self.terminals.insert(terminal_id.clone(), terminal);
        self.mark_session_dirty();
        Some(PaneCreationOutcome {
            workspace_index,
            pane_id,
            terminal_id,
        })
    }

    /// Focuses `pane_id` within workspace `ws_idx`. Pane focus is shared by
    /// every client that views the workspace. False when the pane is not
    /// there or already holds focus.
    pub(crate) fn focus_pane_in_workspace(&mut self, ws_idx: usize, pane_id: PaneId) -> bool {
        let Some(ws) = self.workspaces.get_mut(ws_idx) else {
            return false;
        };
        if !ws.contains_pane(pane_id) || ws.focused_pane_id() == pane_id {
            return false;
        }
        if ws.focus_pane(pane_id) {
            self.mark_session_dirty();
            return true;
        }
        false
    }
}
