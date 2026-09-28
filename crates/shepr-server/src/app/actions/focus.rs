use super::*;

// ---------------------------------------------------------------------------
// Focus tracking
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn resolve_pane_context(
        &self,
        pane_target: Option<(usize, PaneId)>,
        workspace_target: Option<usize>,
        fallback: PaneContextFallback,
    ) -> Option<PaneContext> {
        let (workspace_index, pane_id) = if let Some((workspace_index, pane_id)) = pane_target {
            (workspace_index, pane_id)
        } else if let Some(workspace_index) = workspace_target {
            (
                workspace_index,
                self.workspaces.get(workspace_index)?.focused_pane_id(),
            )
        } else {
            let workspace_index = match fallback {
                PaneContextFallback::None => return None,
                PaneContextFallback::ActiveWorkspace => self.active_index()?,
                PaneContextFallback::WorkspaceCreation => {
                    let selected = self.selected_index();
                    if self.mode == Mode::Navigate {
                        selected.or_else(|| self.active_index())?
                    } else {
                        self.active_index().or(selected)?
                    }
                }
            };
            (
                workspace_index,
                self.workspaces.get(workspace_index)?.focused_pane_id(),
            )
        };
        let tab_index = self
            .workspaces
            .get(workspace_index)?
            .find_tab_index_for_pane(pane_id)?;
        Some(PaneContext {
            workspace_index,
            tab_index,
            pane_id,
        })
    }

    pub(crate) fn current_pane_focus_target(&self) -> Option<PaneFocusTarget> {
        let ws_idx = self.active_index()?;
        let ws = self.workspaces.get(ws_idx)?;
        let pane_id = ws.focused_pane_id();
        Some(PaneFocusTarget {
            workspace_id: shepr_protocol::WorkspaceId::new(ws.id.to_string()),
            pane_id,
        })
    }

    pub(crate) fn commit_workspace_creation(
        &mut self,
        workspace: shepr_mux::workspace::Workspace,
        terminal: shepr_mux::terminal::TerminalState,
        focus: bool,
    ) -> WorkspaceCreationOutcome {
        let workspace_id = workspace.id.to_string();
        let root_pane = workspace.tabs().first().map(|tab| tab.root_pane);
        self.terminals.insert(terminal.id.clone(), terminal);
        self.workspaces.push(workspace);
        let workspace_index = self.workspaces.len() - 1;
        let focused = focus || self.active_index().is_none();
        if focused {
            self.switch_workspace(workspace_index);
            self.mode = Mode::Terminal;
        }
        self.mark_session_dirty();
        WorkspaceCreationOutcome {
            workspace_index,
            workspace_id,
            root_pane,
            focused,
        }
    }

    pub(crate) fn commit_tab_creation(
        &mut self,
        workspace_index: usize,
        tab: shepr_mux::workspace::Tab,
        terminal: shepr_mux::terminal::TerminalState,
        focus: bool,
    ) -> Option<shepr_mux::workspace::TabCreationOutcome> {
        let workspace = self.workspaces.get_mut(workspace_index)?;
        let outcome = workspace.commit_new_tab(tab);
        self.terminals.insert(terminal.id.clone(), terminal);
        if focus {
            self.switch_workspace_tab(workspace_index, outcome.tab_index);
            self.mode = Mode::Terminal;
        }
        self.mark_session_dirty();
        Some(outcome)
    }

    pub(crate) fn commit_layout_tab_creation(
        &mut self,
        workspace_index: usize,
        tab: shepr_mux::workspace::Tab,
        terminals: Vec<shepr_mux::terminal::TerminalState>,
        focus: bool,
    ) -> Option<shepr_mux::workspace::TabCreationOutcome> {
        let outcome = self
            .workspaces
            .get_mut(workspace_index)?
            .commit_new_tab(tab);
        for terminal in terminals {
            self.terminals.insert(terminal.id.clone(), terminal);
        }
        if focus {
            self.switch_workspace_tab(workspace_index, outcome.tab_index);
            self.mode = Mode::Terminal;
        }
        self.mark_session_dirty();
        Some(outcome)
    }

    pub(crate) fn commit_pane_split(
        &mut self,
        workspace_index: usize,
        tab_index: usize,
        pane_id: PaneId,
        prepared_layout: shepr_core::layout::TileLayout,
        terminal: shepr_mux::terminal::TerminalState,
        focus: bool,
        right_click_passthrough: bool,
        previous_focus: Option<PaneFocusTarget>,
    ) -> Option<PaneCreationOutcome> {
        let terminal_id = terminal.id.clone();
        self.workspaces.get_mut(workspace_index)?.commit_new_pane(
            tab_index,
            pane_id,
            prepared_layout,
            terminal_id.clone(),
            focus,
        )?;
        self.terminals.insert(terminal_id.clone(), terminal);
        if let Some(pane) = self
            .workspaces
            .get_mut(workspace_index)
            .and_then(|workspace| workspace.pane_state_mut(pane_id))
        {
            pane.right_click_passthrough = right_click_passthrough;
        }
        if focus {
            self.switch_workspace_tab(workspace_index, tab_index);
            self.record_pane_focus_change(previous_focus, workspace_index, pane_id);
            self.mode = Mode::Terminal;
        }
        self.mark_session_dirty();
        Some(PaneCreationOutcome {
            workspace_index,
            tab_index,
            pane_id,
            terminal_id,
        })
    }

    pub(crate) fn record_pane_focus_change(
        &mut self,
        previous: Option<PaneFocusTarget>,
        ws_idx: usize,
        pane_id: PaneId,
    ) {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        let target = PaneFocusTarget {
            workspace_id: shepr_protocol::WorkspaceId::new(ws.id.to_string()),
            pane_id,
        };
        if previous.as_ref() != Some(&target) {
            self.previous_pane_focus = previous;
        }
    }

    pub(super) fn record_pane_focus_after_navigation(&mut self, previous: Option<PaneFocusTarget>) {
        let current = self.current_pane_focus_target();
        if previous != current {
            self.previous_pane_focus = previous;
        }
    }

    pub(crate) fn focus_pane_in_workspace(&mut self, ws_idx: usize, pane_id: PaneId) -> bool {
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return false;
        };
        let Some(tab_idx) = ws.find_tab_index_for_pane(pane_id) else {
            return false;
        };
        let previous = self.current_pane_focus_target();
        let target = PaneFocusTarget {
            workspace_id: shepr_protocol::WorkspaceId::new(ws.id.to_string()),
            pane_id,
        };
        if previous.as_ref() == Some(&target) {
            return false;
        }

        self.switch_workspace_tab(ws_idx, tab_idx);
        if let Some(tab) = self
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs_mut().get_mut(tab_idx))
        {
            tab.layout.focus_pane(pane_id);
            self.previous_pane_focus = previous;
            self.mark_session_dirty();
            return true;
        }
        false
    }
}
