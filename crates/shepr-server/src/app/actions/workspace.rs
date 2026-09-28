use super::*;

// ---------------------------------------------------------------------------
// Workspace operations
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn next_agent_metadata_expiry(&self) -> Option<std::time::Instant> {
        self.terminals
            .values()
            .filter_map(shepr_mux::terminal::TerminalState::next_agent_metadata_expiry)
            .chain(
                self.terminals
                    .values()
                    .filter_map(|terminal| terminal.metadata_tokens.next_expiry()),
            )
            .chain(
                self.workspaces
                    .iter()
                    .filter_map(|workspace| workspace.metadata_tokens.next_expiry()),
            )
            .min()
    }

    pub(crate) fn expire_agent_metadata_at(
        &mut self,
        scheduled_deadline: std::time::Instant,
        now: std::time::Instant,
    ) -> Vec<PaneStateUpdate> {
        let pane_terminals: Vec<_> = self
            .workspaces
            .iter()
            .flat_map(|ws| {
                let workspace_id = ws.id.to_string();
                ws.tabs().iter().flat_map(move |tab| {
                    let workspace_id = workspace_id.clone();
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| {
                            ws.pane_state(pane_id).map(|pane| {
                                (
                                    workspace_id.clone(),
                                    pane_id,
                                    pane.attached_terminal_id.clone(),
                                )
                            })
                        })
                })
            })
            .collect();
        pane_terminals
            .into_iter()
            .filter_map(|(workspace_id, pane_id, terminal_id)| {
                let mutation = self
                    .terminals
                    .get_mut(&terminal_id)?
                    .expire_agent_metadata_at(scheduled_deadline, now)?;
                let change = mutation.effective_state_change?;
                self.record_agent_state_change_seq(&terminal_id, &change);
                let update = PaneStateUpdate {
                    pane_id,
                    workspace_id,
                    previous: PaneStateSnapshot {
                        agent_label: change.previous_agent_label.clone(),
                        known_agent: change.previous_known_agent,
                        state: change.previous_state,
                        presentation: change.previous_presentation.clone(),
                    },
                    current: PaneStateSnapshot {
                        agent_label: change.agent_label.clone(),
                        known_agent: change.known_agent,
                        state: change.state,
                        presentation: change.presentation.clone(),
                    },
                    cause: PaneStateCause::StateChanged,
                };
                Some(update)
            })
            .collect()
    }

    pub(crate) fn expire_metadata_tokens(
        &mut self,
        now: std::time::Instant,
    ) -> (Vec<(usize, PaneId)>, Vec<usize>) {
        let pane_terminals = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, workspace)| {
                workspace.tabs().iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| {
                            workspace
                                .pane_state(pane_id)
                                .map(|pane| (ws_idx, pane_id, pane.attached_terminal_id.clone()))
                        })
                })
            })
            .collect::<Vec<_>>();
        let changed_panes = pane_terminals
            .into_iter()
            .filter_map(|(ws_idx, pane_id, terminal_id)| {
                let terminal = self.terminals.get_mut(&terminal_id)?;
                terminal.metadata_tokens.expire_at(now).then(|| {
                    terminal.revision = terminal.revision.saturating_add(1);
                    (ws_idx, pane_id)
                })
            })
            .collect();
        let changed_workspaces = self
            .workspaces
            .iter_mut()
            .enumerate()
            .filter_map(|(ws_idx, workspace)| {
                workspace.metadata_tokens.expire_at(now).then_some(ws_idx)
            })
            .collect();
        (changed_panes, changed_workspaces)
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        if idx < self.workspaces.len() {
            let previous_focus = self.current_pane_focus_target();
            self.set_active_index(Some(idx));
            self.set_selected_index(Some(idx));
            let workspace_id = self.workspaces[idx].id.to_string();
            shepr_platform::logging::workspace_focused(&workspace_id);
            self.mark_session_dirty();
            if let Some(ws) = self.workspaces.get_mut(idx) {
                let active_tab = ws.active_tab_index();
                ws.switch_tab(active_tab);
                let tab_id =
                    public_tab_id_for_index(ws, active_tab).unwrap_or_else(|| workspace_id.clone());
                shepr_platform::logging::tab_focused(&workspace_id, &tab_id);
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
        let workspace_id = self.workspaces[ws_idx].id.to_string();
        if workspace_changed {
            shepr_platform::logging::workspace_focused(&workspace_id);
        }
        self.mark_session_dirty();
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.switch_tab(tab_idx);
            let tab_id =
                public_tab_id_for_index(ws, tab_idx).unwrap_or_else(|| workspace_id.clone());
            shepr_platform::logging::tab_focused(&workspace_id, &tab_id);
        }
        self.refresh_active_tab_id();
        self.record_pane_focus_after_navigation(previous_focus);
        true
    }

    #[cfg(test)]
    pub fn switch_tab(&mut self, idx: usize) {
        if let Some(ws_idx) = self.active_index() {
            let previous_focus = self.current_pane_focus_target();
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return;
            };
            ws.switch_tab(idx);
            let workspace_id = ws.id.to_string();
            let tab_id = public_tab_id_for_index(ws, idx).unwrap_or_else(|| workspace_id.clone());
            shepr_platform::logging::tab_focused(&workspace_id, &tab_id);
            self.refresh_active_tab_id();
            self.mark_session_dirty();
            self.record_pane_focus_after_navigation(previous_focus);
        }
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

    pub fn move_workspace_block(
        &mut self,
        workspace_ids: &[String],
        before_workspace_id: Option<&str>,
    ) -> bool {
        let moved_ids = workspace_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        if moved_ids.is_empty()
            || moved_ids.len() != workspace_ids.len()
            || !workspace_ids
                .iter()
                .all(|id| self.workspaces.iter().any(|workspace| workspace.id == *id))
            || before_workspace_id.is_some_and(|id| {
                moved_ids.contains(id)
                    || !self.workspaces.iter().any(|workspace| workspace.id == id)
            })
        {
            return false;
        }

        let mut desired_ids = self
            .workspaces
            .iter()
            .filter(|workspace| !moved_ids.contains(workspace.id.as_str()))
            .map(|workspace| workspace.id.to_string())
            .collect::<Vec<_>>();
        let insert_idx = before_workspace_id
            .and_then(|id| desired_ids.iter().position(|candidate| candidate == id))
            .unwrap_or(desired_ids.len());
        desired_ids.splice(insert_idx..insert_idx, workspace_ids.iter().cloned());
        if self
            .workspaces
            .iter()
            .map(|workspace| workspace.id.as_str())
            .eq(desired_ids.iter().map(String::as_str))
        {
            return false;
        }

        let desired_positions = desired_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect::<std::collections::HashMap<_, _>>();

        self.mark_session_dirty();
        self.workspaces.sort_by_key(|workspace| {
            desired_positions
                .get(workspace.id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
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
            .flat_map(|tab| tab.panes.values())
            .map(|pane| pane.attached_terminal_id.clone())
            .collect()
    }

    pub(crate) fn pane_ids_for_workspace(&self, ws_idx: usize) -> Vec<PaneId> {
        self.workspaces
            .get(ws_idx)
            .into_iter()
            .flat_map(shepr_mux::workspace::Workspace::tabs)
            .flat_map(|tab| tab.layout.pane_ids())
            .collect()
    }

    #[cfg(test)]
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

    pub(crate) fn remove_unattached_terminal_ids(
        &mut self,
        terminal_ids: impl IntoIterator<Item = shepr_protocol::TerminalId>,
    ) {
        for terminal_id in terminal_ids {
            let still_attached = self.workspaces.iter().any(|ws| {
                ws.tabs().iter().any(|tab| {
                    tab.panes
                        .values()
                        .any(|pane| pane.attached_terminal_id == terminal_id)
                })
            });
            if still_attached {
                continue;
            }
            // A direct-attach client normally releases its resize lock on
            // disconnect, but once the terminal is gone the lock guards
            // nothing; drop it here so it cannot outlive the terminal.
            self.direct_attach_resize_locks.remove(&terminal_id);
            if self.terminals.remove(&terminal_id).is_some()
                && !self.terminal_runtime_shutdowns.contains(&terminal_id)
            {
                self.terminal_runtime_shutdowns.push(terminal_id);
            }
        }
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
            tab_index: workspace_plan.tab_index,
            scope: workspace_plan.scope,
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
            });
        }

        self.remove_pane_aliases(&removal.pane_ids);
        self.clear_stale_previous_pane_focus(removal.pane_ids.iter().copied());
        self.remove_unattached_terminal_ids(removal.terminal_ids.iter().cloned());
        if self.active_index() == Some(plan.workspace_index) {
            self.refresh_active_tab_id();
        }
        self.mark_session_dirty();
        PaneRemovalCommit::Removed(PaneRemovalOutcome {
            workspace_index: plan.workspace_index,
            removal,
        })
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

    pub(crate) fn prepare_tab_removal(
        &self,
        workspace_index: usize,
        tab_index: usize,
    ) -> Option<TabRemovalPlan> {
        let workspace = self.workspaces.get(workspace_index)?;
        let tab_number = workspace.tabs().get(tab_index)?.number;
        Some(TabRemovalPlan {
            workspace_index,
            tab_index,
            scope: if workspace.tabs().len() <= 1 {
                TabRemovalScope::Workspace
            } else {
                TabRemovalScope::Tab
            },
            workspace_id: workspace.id.to_string(),
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
            || tab.number != plan.tab_number
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
        self.remove_pane_aliases(&removal.pane_ids);
        self.clear_stale_previous_pane_focus(removal.pane_ids.iter().copied());
        self.remove_unattached_terminal_ids(removal.terminal_ids.iter().cloned());
        self.mark_session_dirty();
        let tab_id = shepr_mux::workspace::public_tab_id_for_number(
            &removal.workspace_id,
            removal.tab_number,
        );
        shepr_platform::logging::tab_closed(&removal.workspace_id, &tab_id);
        TabRemovalCommit::Removed(TabRemovalOutcome {
            workspace_index: plan.workspace_index,
            scope: plan.scope,
            pane_ids: removal.pane_ids.clone(),
            terminal_ids: removal.terminal_ids.clone(),
            tab: Some(removal),
        })
    }

    #[cfg(test)]
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

    /// Drops the public pane-id aliases that point at any of `pane_ids`.
    pub(crate) fn remove_pane_aliases(&mut self, pane_ids: &[PaneId]) {
        self.public_pane_id_aliases
            .retain(|_, alias| !pane_ids.contains(alias));
    }

    #[cfg(test)]
    pub fn close_selected_workspace(&mut self) {
        self.close_workspace_at(self.selected_index().unwrap_or(0));
    }

    /// Closes the workspace at `ws_idx` and everything it owns.
    ///
    /// Focus stays on the previously active workspace and the sidebar cursor
    /// on the previously selected one when they survive; otherwise both fall
    /// back to the closed slot (clamped). Closing the last workspace leaves
    /// terminal mode, since there is no pane left to type into.
    pub(crate) fn close_workspace_at(&mut self, ws_idx: usize) -> Option<WorkspaceRemovalOutcome> {
        let workspace_id = self.workspaces.get(ws_idx).map(|ws| ws.id.to_string())?;
        self.mark_session_dirty();
        shepr_platform::logging::workspace_closed(&workspace_id);

        let terminal_ids = self.terminal_ids_for_workspace(ws_idx);
        let pane_ids = self.pane_ids_for_workspace(ws_idx);
        let active_workspace_id = self.active.clone();
        let selected_workspace_id = self.selected.clone();

        self.remove_pane_aliases(&pane_ids);
        self.clear_stale_previous_pane_focus(pane_ids.iter().copied());
        self.workspaces.remove(ws_idx);
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
        })
    }
}
