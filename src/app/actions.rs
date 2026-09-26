//! Pure state mutations on AppState.
//! These don't need channels, async, or PTY runtime.

use std::time::Instant;

use tracing::debug;

use crate::detect::{Agent, AgentState};
use crate::events::AppEvent;
use crate::layout::PaneId;
#[cfg(test)]
use crate::layout::{NavDirection, find_in_direction};
use crate::terminal::{EffectiveStateChange, TerminalStateMutation};
use crate::workspace::WorkspaceGitStatus;

use super::state::{AppState, Mode, PaneFocusTarget};

fn public_tab_id_for_index(ws: &crate::workspace::Workspace, tab_idx: usize) -> Option<String> {
    let tab_number = ws.public_tab_number(tab_idx)?;
    Some(crate::workspace::public_tab_id_for_number(
        &ws.id, tab_number,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneStateUpdate {
    pub pane_id: PaneId,
    pub workspace_id: String,
    pub previous_agent_label: Option<String>,
    pub previous_known_agent: Option<Agent>,
    pub previous_state: AgentState,
    pub previous_presentation: crate::terminal::EffectivePresentation,
    pub agent_label: Option<String>,
    pub known_agent: Option<Agent>,
    pub state: AgentState,
    pub presentation: crate::terminal::EffectivePresentation,
    pub agent_name_changed: bool,
    pub agent_released: bool,
}

// ---------------------------------------------------------------------------
// Focus tracking
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn current_pane_focus_target(&self) -> Option<PaneFocusTarget> {
        let ws_idx = self.active?;
        let ws = self.workspaces.get(ws_idx)?;
        let pane_id = ws.focused_pane_id()?;
        Some(PaneFocusTarget {
            workspace_id: ws.id.clone(),
            pane_id,
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
            workspace_id: ws.id.clone(),
            pane_id,
        };
        if previous.as_ref() != Some(&target) {
            self.previous_pane_focus = previous;
        }
    }

    fn record_pane_focus_after_navigation(&mut self, previous: Option<PaneFocusTarget>) {
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
            workspace_id: ws.id.clone(),
            pane_id,
        };
        if previous.as_ref() == Some(&target) {
            return false;
        }

        self.switch_workspace_tab(ws_idx, tab_idx);
        if let Some(tab) = self
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))
        {
            tab.layout.focus_pane(pane_id);
            self.previous_pane_focus = previous;
            self.mark_session_dirty();
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Workspace operations
// ---------------------------------------------------------------------------

impl AppState {
    pub(crate) fn next_agent_metadata_expiry(&self) -> Option<std::time::Instant> {
        self.terminals
            .values()
            .filter_map(crate::terminal::TerminalState::next_agent_metadata_expiry)
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
                let workspace_id = ws.id.clone();
                ws.tabs.iter().flat_map(move |tab| {
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
                    previous_agent_label: change.previous_agent_label.clone(),
                    previous_known_agent: change.previous_known_agent,
                    previous_state: change.previous_state,
                    previous_presentation: change.previous_presentation.clone(),
                    agent_label: change.agent_label.clone(),
                    known_agent: change.known_agent,
                    state: change.state,
                    presentation: change.presentation.clone(),
                    agent_name_changed: false,
                    agent_released: false,
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
                workspace.tabs.iter().flat_map(move |tab| {
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
            self.active = Some(idx);
            self.selected = idx;
            let workspace_id = self.workspaces[idx].id.clone();
            crate::logging::workspace_focused(&workspace_id);
            self.mark_session_dirty();
            if let Some(ws) = self.workspaces.get_mut(idx) {
                let active_tab = ws.active_tab;
                ws.switch_tab(active_tab);
                let tab_id =
                    public_tab_id_for_index(ws, active_tab).unwrap_or_else(|| workspace_id.clone());
                crate::logging::tab_focused(&workspace_id, &tab_id);
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
            .is_none_or(|ws| tab_idx >= ws.tabs.len())
        {
            return false;
        }

        let previous_focus = self.current_pane_focus_target();
        let workspace_changed = self.active != Some(ws_idx);
        self.active = Some(ws_idx);
        self.selected = ws_idx;
        let workspace_id = self.workspaces[ws_idx].id.clone();
        if workspace_changed {
            crate::logging::workspace_focused(&workspace_id);
        }
        self.mark_session_dirty();
        if let Some(ws) = self.workspaces.get_mut(ws_idx) {
            ws.switch_tab(tab_idx);
            let tab_id =
                public_tab_id_for_index(ws, tab_idx).unwrap_or_else(|| workspace_id.clone());
            crate::logging::tab_focused(&workspace_id, &tab_id);
        }
        self.record_pane_focus_after_navigation(previous_focus);
        true
    }

    #[cfg(test)]
    pub fn switch_tab(&mut self, idx: usize) {
        if let Some(ws_idx) = self.active {
            let previous_focus = self.current_pane_focus_target();
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return;
            };
            ws.switch_tab(idx);
            let workspace_id = ws.id.clone();
            let tab_id = public_tab_id_for_index(ws, idx).unwrap_or_else(|| workspace_id.clone());
            crate::logging::tab_focused(&workspace_id, &tab_id);
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

        let active_id = self.active.map(|idx| self.workspaces[idx].id.clone());
        let selected_id = self
            .workspaces
            .get(self.selected)
            .map(|workspace| workspace.id.clone());

        let workspace = self.workspaces.remove(source_idx);
        self.workspaces.insert(target_idx, workspace);

        self.active = active_id.and_then(|id| self.workspaces.iter().position(|ws| ws.id == id));
        self.selected = selected_id
            .and_then(|id| self.workspaces.iter().position(|ws| ws.id == id))
            .unwrap_or(0);
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
            .map(|workspace| workspace.id.clone())
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

        let active_id = self.active.map(|idx| self.workspaces[idx].id.clone());
        let selected_id = self
            .workspaces
            .get(self.selected)
            .map(|workspace| workspace.id.clone());
        let desired_positions = desired_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect::<std::collections::HashMap<_, _>>();

        self.mark_session_dirty();
        self.workspaces.sort_by_key(|workspace| {
            desired_positions
                .get(&workspace.id)
                .copied()
                .unwrap_or(usize::MAX)
        });
        self.active = active_id.and_then(|id| self.workspaces.iter().position(|ws| ws.id == id));
        self.selected = selected_id
            .and_then(|id| self.workspaces.iter().position(|ws| ws.id == id))
            .unwrap_or(0);
        true
    }

    pub(crate) fn terminal_ids_for_workspace(
        &self,
        ws_idx: usize,
    ) -> Vec<crate::terminal::TerminalId> {
        self.workspaces
            .get(ws_idx)
            .into_iter()
            .flat_map(|ws| &ws.tabs)
            .flat_map(|tab| tab.panes.values())
            .map(|pane| pane.attached_terminal_id.clone())
            .collect()
    }

    pub(crate) fn pane_ids_for_workspace(&self, ws_idx: usize) -> Vec<PaneId> {
        self.workspaces
            .get(ws_idx)
            .into_iter()
            .flat_map(|ws| &ws.tabs)
            .flat_map(|tab| tab.layout.pane_ids())
            .collect()
    }

    pub(crate) fn terminal_ids_for_tab(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Vec<crate::terminal::TerminalId> {
        self.workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs.get(tab_idx))
            .into_iter()
            .flat_map(|tab| tab.panes.values())
            .map(|pane| pane.attached_terminal_id.clone())
            .collect()
    }

    pub(crate) fn pane_ids_for_tab(&self, ws_idx: usize, tab_idx: usize) -> Vec<PaneId> {
        self.workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs.get(tab_idx))
            .map(|tab| tab.layout.pane_ids())
            .unwrap_or_default()
    }

    pub(crate) fn terminal_id_for_pane(
        &self,
        ws_idx: usize,
        pane_id: PaneId,
    ) -> Option<crate::terminal::TerminalId> {
        self.workspaces
            .get(ws_idx)?
            .pane_state(pane_id)
            .map(|pane| pane.attached_terminal_id.clone())
    }

    pub(crate) fn remove_unattached_terminal_ids(
        &mut self,
        terminal_ids: impl IntoIterator<Item = crate::terminal::TerminalId>,
    ) {
        for terminal_id in terminal_ids {
            let still_attached = self.workspaces.iter().any(|ws| {
                ws.tabs.iter().any(|tab| {
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
        self.close_workspace_at(self.selected);
    }

    /// Closes the workspace at `ws_idx` and everything it owns.
    ///
    /// Focus stays on the previously active workspace and the sidebar cursor
    /// on the previously selected one when they survive; otherwise both fall
    /// back to the closed slot (clamped). Closing the last workspace leaves
    /// terminal mode, since there is no pane left to type into.
    pub(crate) fn close_workspace_at(&mut self, ws_idx: usize) {
        let Some(workspace_id) = self.workspaces.get(ws_idx).map(|ws| ws.id.clone()) else {
            return;
        };
        self.mark_session_dirty();
        crate::logging::workspace_closed(&workspace_id);

        let terminal_ids = self.terminal_ids_for_workspace(ws_idx);
        let pane_ids = self.pane_ids_for_workspace(ws_idx);
        let active_workspace_id = self
            .active
            .and_then(|idx| self.workspaces.get(idx))
            .map(|ws| ws.id.clone());
        let selected_workspace_id = self.workspaces.get(self.selected).map(|ws| ws.id.clone());

        self.remove_pane_aliases(&pane_ids);
        self.clear_stale_previous_pane_focus(pane_ids);
        self.workspaces.remove(ws_idx);
        self.remove_unattached_terminal_ids(terminal_ids);

        if self.workspaces.is_empty() {
            self.active = None;
            self.selected = 0;
            if self.mode == Mode::Terminal {
                self.mode = Mode::Navigate;
            }
            return;
        }
        let last = self.workspaces.len() - 1;
        let position_of = |id: Option<String>, workspaces: &[crate::workspace::Workspace]| {
            id.and_then(|id| workspaces.iter().position(|ws| ws.id == id))
        };
        let active = position_of(active_workspace_id, &self.workspaces).unwrap_or(ws_idx.min(last));
        self.active = Some(active);
        self.selected = position_of(selected_workspace_id, &self.workspaces).unwrap_or(active);
    }
}

// ---------------------------------------------------------------------------
// Pane operations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneZoomCommand {
    Toggle,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneZoomNoopReason {
    SinglePane,
    AlreadyZoomed,
    AlreadyUnzoomed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneZoomOutcome {
    pub changed: bool,
    pub focus_changed: bool,
    pub reason: Option<PaneZoomNoopReason>,
    pub zoomed: bool,
}

impl AppState {
    #[cfg(test)]
    pub fn navigate_pane(&mut self, direction: NavDirection) {
        let Some(ws_idx) = self.active else {
            return;
        };
        let Some(tab) = self.workspaces.get(ws_idx).and_then(|ws| ws.active_tab()) else {
            return;
        };
        let panes = if tab.zoomed {
            tab.layout.panes(self.view.terminal_area)
        } else {
            self.view.pane_infos.clone()
        };

        if let Some(focused) = panes.iter().find(|p| p.is_focused)
            && let Some(target) = find_in_direction(focused, direction, &panes)
        {
            self.focus_pane_in_workspace(ws_idx, target);
        }
    }

    #[cfg(test)]
    pub fn swap_pane(&mut self, direction: NavDirection) -> bool {
        let Some(ws_idx) = self.active else {
            return false;
        };
        let Some(tab) = self.workspaces.get(ws_idx).and_then(|ws| ws.active_tab()) else {
            return false;
        };
        let panes = if tab.zoomed {
            tab.layout.panes(self.view.terminal_area)
        } else {
            self.view.pane_infos.clone()
        };

        let Some(focused) = panes.iter().find(|p| p.is_focused) else {
            return false;
        };
        let Some(target) = find_in_direction(focused, direction, &panes) else {
            return false;
        };
        let source = focused.id;
        let Some(tab) = self
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.active_tab_mut())
        else {
            return false;
        };
        if tab.layout.swap_panes(source, target) {
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
            if let Some(tab) = self
                .active
                .and_then(|i| self.workspaces.get_mut(i))
                .and_then(|ws| ws.active_tab_mut())
            {
                tab.layout.resize_focused(direction, 0.05, area);
                self.mark_session_dirty();
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
        let tab = self
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs.get_mut(tab_idx))?;
        if tab.layout.pane_count() <= 1 {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
                reason: Some(PaneZoomNoopReason::SinglePane),
                zoomed: tab.zoomed,
            });
        }

        let desired = match command {
            PaneZoomCommand::Toggle => !tab.zoomed,
            PaneZoomCommand::On => true,
            PaneZoomCommand::Off => false,
        };
        let reason = match (command, tab.zoomed) {
            (PaneZoomCommand::On, true) => Some(PaneZoomNoopReason::AlreadyZoomed),
            (PaneZoomCommand::Off, false) => Some(PaneZoomNoopReason::AlreadyUnzoomed),
            _ => None,
        };
        if reason.is_some() {
            return Some(PaneZoomOutcome {
                changed: false,
                focus_changed,
                reason,
                zoomed: tab.zoomed,
            });
        }

        tab.zoomed = desired;
        let zoomed = tab.zoomed;
        self.mark_session_dirty();
        Some(PaneZoomOutcome {
            changed: true,
            focus_changed,
            reason: None,
            zoomed,
        })
    }

    #[cfg(test)]
    pub fn toggle_zoom(&mut self) {
        let Some(ws_idx) = self.active else {
            return;
        };
        let Some(pane_id) = self
            .workspaces
            .get(ws_idx)
            .and_then(crate::workspace::Workspace::focused_pane_id)
        else {
            return;
        };
        self.apply_pane_zoom(ws_idx, pane_id, PaneZoomCommand::Toggle);
    }

    #[cfg(test)]
    /// Close the focused pane. Returns true when the close was deferred to confirmation.
    pub fn close_pane(&mut self) -> bool {
        let active = self.active;
        self.mark_session_dirty();
        let terminal_ids = active
            .and_then(|i| {
                self.workspaces
                    .get(i)
                    .and_then(|ws| ws.focused_pane_id().map(|pane_id| (i, pane_id)))
            })
            .and_then(|(i, pane_id)| self.terminal_id_for_pane(i, pane_id))
            .into_iter()
            .collect::<Vec<_>>();
        let pane_ids = active
            .and_then(|i| {
                self.workspaces
                    .get(i)
                    .and_then(crate::workspace::Workspace::focused_pane_id)
            })
            .into_iter()
            .collect::<Vec<_>>();
        let should_close_workspace = active
            .and_then(|i| self.workspaces.get_mut(i))
            .is_some_and(crate::workspace::Workspace::close_focused);
        self.clear_stale_previous_pane_focus(pane_ids);
        if should_close_workspace {
            if let Some(active) = active {
                self.selected = active;
            }
            self.close_selected_workspace();
        } else {
            self.remove_unattached_terminal_ids(terminal_ids);
        }
        false
    }

    #[cfg(test)]
    /// Close the active tab. Returns true when the close was deferred to confirmation.
    pub fn close_tab(&mut self) -> bool {
        self.mark_session_dirty();
        let should_close_workspace = self
            .active
            .and_then(|i| self.workspaces.get(i))
            .is_some_and(|ws| ws.tabs.len() <= 1);
        if should_close_workspace {
            if let Some(active) = self.active {
                self.selected = active;
            }
            self.close_selected_workspace();
            return false;
        }
        if let Some(ws_idx) = self.active {
            let terminal_ids = self
                .workspaces
                .get(ws_idx)
                .map(|ws| self.terminal_ids_for_tab(ws_idx, ws.active_tab))
                .unwrap_or_default();
            let pane_ids = self
                .workspaces
                .get(ws_idx)
                .map(|ws| self.pane_ids_for_tab(ws_idx, ws.active_tab))
                .unwrap_or_default();
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return false;
            };
            let workspace_id = ws.id.clone();
            let closing_tab_id =
                public_tab_id_for_index(ws, ws.active_tab).unwrap_or_else(|| workspace_id.clone());
            ws.close_active_tab();
            self.clear_stale_previous_pane_focus(pane_ids);
            self.remove_unattached_terminal_ids(terminal_ids);
            crate::logging::tab_closed(&workspace_id, &closing_tab_id);
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

impl AppState {
    pub fn apply_workspace_git_statuses(
        &mut self,
        terminal_runtimes: &crate::terminal::TerminalRuntimeRegistry,
        results: Vec<WorkspaceGitStatus>,
    ) -> bool {
        let mut changed = false;
        for result in results {
            let Some(ws_idx) = self
                .workspaces
                .iter()
                .position(|ws| ws.id == result.workspace_id)
            else {
                continue;
            };

            if self.workspaces[ws_idx]
                .resolved_identity_cwd_from(&self.terminals, terminal_runtimes)
                .as_ref()
                != Some(&result.resolved_identity_cwd)
            {
                continue;
            }

            let ws = &mut self.workspaces[ws_idx];
            if ws.cached_identity_cwd != result.resolved_identity_cwd {
                ws.cached_identity_cwd = result.resolved_identity_cwd;
            }
            if ws.cached_auto_label != result.auto_label {
                ws.cached_auto_label = result.auto_label;
                changed |= ws.custom_name.is_none();
            }
            if ws.cached_git_status_key != result.status_cache_key {
                ws.cached_git_status_key = result.status_cache_key;
            }
            if result.demand.branch && ws.cached_git_branch != result.branch {
                ws.cached_git_branch = result.branch;
                changed = true;
            }
            if result.demand.ahead_behind && ws.cached_git_ahead_behind != result.ahead_behind {
                ws.cached_git_ahead_behind = result.ahead_behind;
                changed = true;
            }
            if ws.cached_git_space != result.space {
                ws.cached_git_space = result.space;
                changed = true;
            }
        }
        changed
    }

    pub fn handle_app_event(&mut self, event: AppEvent) -> Vec<PaneStateUpdate> {
        match event {
            AppEvent::PaneDied { pane_id, .. } => {
                self.handle_pane_died(pane_id);
                Vec::new()
            }
            AppEvent::AgentProcessDetected {
                pane_id,
                agent,
                observed_at,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    Some(terminal.set_detected_agent_process_at(agent, observed_at))
                })
                .into_iter()
                .collect(),
            AppEvent::AgentPromptObserved {
                pane_id,
                agent,
                ready,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    terminal.observe_agent_prompt_ready(agent, ready)
                })
                .into_iter()
                .collect(),
            AppEvent::StateChanged {
                pane_id,
                agent,
                state,
                visible_blocker,
                process_exited,
                observed_at,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    Some(terminal.set_detected_state_with_screen_signals_at(
                        agent,
                        state,
                        visible_blocker,
                        process_exited,
                        observed_at,
                    ))
                })
                .into_iter()
                .collect(),
            AppEvent::HookStateReported {
                pane_id,
                source,
                agent_label,
                state,
                message,
                seq,
                session_ref,
            } => {
                if crate::agent_resume::is_reserved_native_state_source(&source, &agent_label) {
                    self.update_terminal_state(pane_id, |terminal| {
                        terminal.set_agent_session_ref(source, agent_label, session_ref, seq)
                    })
                    .into_iter()
                    .collect()
                } else {
                    self.update_terminal_state(pane_id, |terminal| {
                        terminal.set_hook_authority_with_session_ref(
                            source,
                            agent_label,
                            state,
                            message,
                            session_ref,
                            seq,
                        )
                    })
                    .into_iter()
                    .collect()
                }
            }
            AppEvent::AgentSessionReported {
                pane_id,
                source,
                agent_label,
                seq,
                session_ref,
                session_start_source,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    terminal.set_agent_session_ref_for_typed_start_source(
                        source,
                        agent_label,
                        session_ref,
                        seq,
                        session_start_source,
                    )
                })
                .into_iter()
                .collect(),
            AppEvent::HookMetadataReported {
                pane_id,
                source,
                agent_label,
                applies_to_source,
                title,
                display_agent,
                state_labels,
                clear_title,
                clear_display_agent,
                clear_state_labels,
                seq,
                ttl,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    terminal.set_agent_metadata(crate::terminal::AgentMetadataReport {
                        source,
                        agent_label,
                        applies_to_source,
                        title,
                        display_agent,
                        state_labels,
                        clear_title,
                        clear_display_agent,
                        clear_state_labels,
                        ttl,
                        seq,
                    })
                })
                .into_iter()
                .collect(),
            AppEvent::HookAuthorityCleared {
                pane_id,
                source,
                seq,
            } => self
                .update_terminal_state(pane_id, |terminal| {
                    terminal.clear_hook_authority_with_mutation(source.as_deref(), seq)
                })
                .into_iter()
                .collect(),
            AppEvent::HookAgentReleased {
                pane_id,
                source,
                agent_label,
                seq,
                ..
            } => {
                if crate::agent_resume::is_official_agent_source(&source, &agent_label) {
                    Vec::new()
                } else {
                    self.update_terminal_state(pane_id, |terminal| {
                        terminal.release_agent_with_mutation(&source, &agent_label, seq)
                    })
                    .into_iter()
                    .collect()
                }
            }
            // Host-local effects are intercepted by HeadlessServer and forwarded to the
            // foreground client; they never touch AppState. Kept for AppEvent exhaustiveness.
            AppEvent::ClipboardWrite { .. } => Vec::new(),
            AppEvent::TerminalCwdReported { pane_id, cwd } => {
                // The PTY reader thread that publishes this event has already
                // checked that the path is an existing directory. Repeating
                // that `stat` here would put filesystem IO (possibly a hung
                // network mount) on the loop that fans frames out to every
                // client, so only the pure shape check is kept.
                if !cwd.is_absolute() {
                    return Vec::new();
                }
                let Some(terminal_id) = self.workspaces.iter().find_map(|ws| {
                    ws.pane_state(pane_id)
                        .map(|pane| pane.attached_terminal_id.clone())
                }) else {
                    return Vec::new();
                };
                let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
                    return Vec::new();
                };
                if terminal.cwd != cwd {
                    terminal.cwd = cwd;
                    self.mark_session_dirty();
                }
                Vec::new()
            }
            AppEvent::GitStatusRefreshed {
                results,
                cache_updates,
            } => {
                let _ = results;
                let _ = cache_updates;
                Vec::new()
            }
            AppEvent::TabBarCommandFinished { .. } => Vec::new(),
        }
    }

    fn update_terminal_state<F>(&mut self, pane_id: PaneId, update: F) -> Option<PaneStateUpdate>
    where
        F: FnOnce(&mut crate::terminal::TerminalState) -> Option<TerminalStateMutation>,
    {
        let ws_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.pane_state(pane_id).is_some())?;
        let workspace_id = self.workspaces[ws_idx].id.clone();
        let terminal_id = self.workspaces[ws_idx]
            .pane_state(pane_id)?
            .attached_terminal_id
            .clone();
        let now = Instant::now();
        let (mutation, managed_changed, agent_name_changed, unchanged_change) = {
            let terminal = self.terminals.get_mut(&terminal_id)?;
            let previous_agent_name = terminal.agent_name.clone();
            let mutation = update(terminal)?;
            let managed_changed = terminal.reconcile_managed_agent_at(now, false);
            let agent_name_changed = terminal.agent_name != previous_agent_name;
            let unchanged_change = (mutation.agent_released || agent_name_changed)
                .then(|| terminal.unchanged_effective_state_change_at(now));
            (
                mutation,
                managed_changed,
                agent_name_changed,
                unchanged_change,
            )
        };
        if mutation.session_ref_changed || managed_changed || agent_name_changed {
            self.mark_session_dirty();
        }
        let agent_released = mutation.agent_released;
        let change = mutation.effective_state_change.or(unchanged_change)?;
        self.record_agent_state_change_seq(&terminal_id, &change);
        let update = PaneStateUpdate {
            pane_id,
            workspace_id,
            previous_agent_label: change.previous_agent_label.clone(),
            previous_known_agent: change.previous_known_agent,
            previous_state: change.previous_state,
            previous_presentation: change.previous_presentation.clone(),
            agent_label: if agent_released {
                change.previous_agent_label.clone()
            } else {
                change.agent_label.clone()
            },
            known_agent: if agent_released {
                change.previous_known_agent
            } else {
                change.known_agent
            },
            state: change.state,
            presentation: change.presentation.clone(),
            agent_name_changed,
            agent_released,
        };
        Some(update)
    }

    /// The old background-completion predicate only populated the removed
    /// completion sequence; it had no notification caller. Pane status is now
    /// the current state directly, while state-change sequences remain so API
    /// waiters and endpoint agent sorting can observe transitions between
    /// snapshots.
    fn record_agent_state_change_seq(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        change: &EffectiveStateChange,
    ) {
        if change.previous_state == change.state {
            return;
        }
        self.next_agent_state_change_seq += 1;
        if let Some(terminal) = self.terminals.get_mut(terminal_id) {
            terminal.last_agent_state_change_seq = Some(self.next_agent_state_change_seq);
        }
    }

    pub(crate) fn next_managed_agent_deadline(&self) -> Option<Instant> {
        self.terminals
            .values()
            .filter_map(crate::terminal::TerminalState::next_managed_agent_deadline)
            .min()
    }

    pub(crate) fn publish_pane_process_exit_if_agent(
        &mut self,
        pane_id: PaneId,
    ) -> Option<PaneStateUpdate> {
        let observed_at = std::time::Instant::now();
        let update = self.update_terminal_state(pane_id, |terminal| {
            let agent = terminal.effective_known_agent().or(terminal.detected_agent);
            if agent.is_none() && !terminal.full_lifecycle_hook_authority_active() {
                return None;
            }
            Some(terminal.set_detected_state_with_screen_signals_at(
                agent,
                AgentState::Idle,
                false,
                true,
                observed_at,
            ))
        })?;
        update.agent_released.then_some(update)
    }

    fn handle_pane_died(&mut self, pane_id: PaneId) {
        self.clear_stale_previous_pane_focus([pane_id]);
        let ws_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.find_tab_index_for_pane(pane_id).is_some());

        let Some(ws_idx) = ws_idx else {
            // Expected, not a fault: a pane already removed because its PTY
            // reader reported a broken terminal core gets a second PaneDied
            // from the child watcher once the child is reaped.
            debug!(pane = pane_id.raw(), "PaneDied for unknown pane");
            return;
        };

        let pane_terminal_id = self.terminal_id_for_pane(ws_idx, pane_id);
        // Final-pane closure goes through `close_workspace_at`, which removes
        // aliases for the whole workspace. Other deaths prune this pane's
        // aliases directly below.
        if self.workspaces[ws_idx].close_pane(pane_id) {
            self.close_workspace_at(ws_idx);
            return;
        }
        self.remove_pane_aliases(&[pane_id]);
        self.mark_session_dirty();
        self.remove_unattached_terminal_ids(pane_terminal_id);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;
    use ratatui::layout::Direction;

    fn app_with_workspaces(names: &[&str]) -> AppState {
        let mut state = AppState::test_new();
        for name in names {
            let ws = Workspace::test_new(name);
            state.workspaces.push(ws);
        }
        state.ensure_test_terminals();
        if !state.workspaces.is_empty() {
            state.active = Some(0);
            state.mode = Mode::Terminal;
        }
        state
    }

    #[test]
    fn apply_workspace_git_statuses_updates_matching_workspace() {
        let mut state = app_with_workspaces(&["one", "two"]);
        let first_id = state.workspaces[0].id.clone();
        let first_cwd = state.workspaces[0]
            .resolved_identity_cwd()
            .expect("test precondition");
        let second_id = state.workspaces[1].id.clone();

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let changed = state.apply_workspace_git_statuses(
            &terminal_runtimes,
            vec![WorkspaceGitStatus {
                workspace_id: first_id,
                resolved_identity_cwd: first_cwd.clone(),
                status_cache_key: first_cwd,
                demand: crate::workspace::GitStatusRefreshDemand::ALL,
                auto_label: "one".into(),
                branch: Some("main".into()),
                ahead_behind: Some((2, 1)),
                space: None,
            }],
        );

        assert!(changed);
        assert_eq!(state.workspaces[0].branch().as_deref(), Some("main"));
        assert_eq!(state.workspaces[0].git_ahead_behind(), Some((2, 1)));
        assert_eq!(state.workspaces[1].id, second_id);
        assert_eq!(state.workspaces[1].git_ahead_behind(), None);
    }

    #[test]
    fn apply_workspace_git_statuses_ignores_stale_cwd() {
        let mut state = app_with_workspaces(&["one"]);
        let workspace_id = state.workspaces[0].id.clone();
        state.workspaces[0].cached_git_branch = Some("old".into());
        state.workspaces[0].cached_git_ahead_behind = Some((1, 0));

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let changed = state.apply_workspace_git_statuses(
            &terminal_runtimes,
            vec![WorkspaceGitStatus {
                workspace_id,
                resolved_identity_cwd: std::path::PathBuf::from("/definitely/not/current"),
                status_cache_key: std::path::PathBuf::from("/definitely/not/current"),
                demand: crate::workspace::GitStatusRefreshDemand::ALL,
                auto_label: "stale".into(),
                branch: Some("main".into()),
                ahead_behind: Some((0, 1)),
                space: None,
            }],
        );

        assert!(!changed);
        assert_eq!(state.workspaces[0].branch().as_deref(), Some("old"));
        assert_eq!(state.workspaces[0].git_ahead_behind(), Some((1, 0)));
    }

    #[test]
    fn apply_workspace_git_statuses_ignores_unrequested_branch_changes() {
        let mut state = app_with_workspaces(&["one"]);
        let workspace_id = state.workspaces[0].id.clone();
        let cwd = state.workspaces[0]
            .resolved_identity_cwd()
            .expect("test precondition");
        state.workspaces[0].cached_auto_label = "one".into();
        state.workspaces[0].cached_git_branch = Some("old".into());

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let changed = state.apply_workspace_git_statuses(
            &terminal_runtimes,
            vec![WorkspaceGitStatus {
                workspace_id,
                resolved_identity_cwd: cwd.clone(),
                status_cache_key: cwd,
                demand: crate::workspace::GitStatusRefreshDemand {
                    branch: false,
                    ahead_behind: true,
                },
                auto_label: "one".into(),
                branch: Some("new".into()),
                ahead_behind: None,
                space: None,
            }],
        );

        assert!(!changed);
        assert_eq!(state.workspaces[0].branch().as_deref(), Some("old"));
    }

    #[test]
    fn apply_workspace_git_statuses_clears_missing_git_status() {
        let mut state = app_with_workspaces(&["one"]);
        let workspace_id = state.workspaces[0].id.clone();
        let cwd = state.workspaces[0]
            .resolved_identity_cwd()
            .expect("test precondition");
        state.workspaces[0].cached_git_branch = Some("main".into());
        state.workspaces[0].cached_git_ahead_behind = Some((1, 2));

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let changed = state.apply_workspace_git_statuses(
            &terminal_runtimes,
            vec![WorkspaceGitStatus {
                workspace_id,
                resolved_identity_cwd: cwd.clone(),
                status_cache_key: cwd,
                demand: crate::workspace::GitStatusRefreshDemand::ALL,
                auto_label: "one".into(),
                branch: None,
                ahead_behind: None,
                space: None,
            }],
        );

        assert!(changed);
        assert_eq!(state.workspaces[0].branch(), None);
        assert_eq!(state.workspaces[0].git_ahead_behind(), None);
    }

    #[test]
    fn switch_workspace_updates_active_and_selected() {
        let mut state = app_with_workspaces(&["a", "b", "c"]);
        state.switch_workspace(2);
        assert_eq!(state.active, Some(2));
        assert_eq!(state.selected, 2);
    }

    #[test]
    fn switch_workspace_out_of_bounds_is_noop() {
        let mut state = app_with_workspaces(&["a"]);
        state.switch_workspace(5);
        assert_eq!(state.active, Some(0));
    }

    #[test]
    fn move_workspace_reorders_without_changing_logical_selection() {
        let mut state = app_with_workspaces(&["a", "b", "c"]);
        let active_id = state.workspaces[1].id.clone();
        let selected_id = state.workspaces[2].id.clone();
        state.active = Some(1);
        state.selected = 2;

        state.move_workspace(1, 0);

        let names: Vec<_> = state
            .workspaces
            .iter()
            .map(crate::workspace::Workspace::display_name)
            .collect();
        assert_eq!(names, vec!["b", "a", "c"]);
        assert_eq!(state.active, Some(0));
        assert_eq!(state.selected, 2);
        assert_eq!(
            state.workspaces[state.active.expect("test precondition")].id,
            active_id
        );
        assert_eq!(state.workspaces[state.selected].id, selected_id);
    }

    #[test]
    fn move_workspace_accepts_insert_at_end() {
        let mut state = app_with_workspaces(&["a", "b", "c"]);

        state.move_workspace(0, state.workspaces.len());

        let names: Vec<_> = state
            .workspaces
            .iter()
            .map(crate::workspace::Workspace::display_name)
            .collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn move_workspace_block_collects_non_contiguous_members() {
        let mut state =
            app_with_workspaces(&["child-one", "normal", "parent", "child-two", "tail"]);
        let parent_id = state.workspaces[2].id.clone();
        let child_one_id = state.workspaces[0].id.clone();
        let child_two_id = state.workspaces[3].id.clone();
        let tail_id = state.workspaces[4].id.clone();
        state.active = Some(0);
        state.selected = 4;

        assert!(state.move_workspace_block(
            &[parent_id, child_one_id.clone(), child_two_id],
            Some(&tail_id),
        ));

        let names = state
            .workspaces
            .iter()
            .map(crate::workspace::Workspace::display_name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["normal", "parent", "child-one", "child-two", "tail"]
        );
        assert_eq!(
            state.workspaces[state.active.expect("test precondition")].id,
            child_one_id
        );
        assert_eq!(state.workspaces[state.selected].id, tail_id);
    }

    #[test]
    fn move_workspace_block_rejects_invalid_and_noop_orders() {
        let mut state = app_with_workspaces(&["a", "b", "c"]);
        let ids = state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();

        assert!(!state.move_workspace_block(&[], None));
        assert!(!state.move_workspace_block(&[ids[0].clone(), ids[0].clone()], None));
        assert!(!state.move_workspace_block(&["missing".into()], None));
        assert!(!state.move_workspace_block(&[ids[0].clone()], Some(&ids[0])));
        assert!(!state.move_workspace_block(&[ids[0].clone()], Some(&ids[1])));
        assert_eq!(
            state
                .workspaces
                .iter()
                .map(crate::workspace::Workspace::display_name)
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn close_workspace_adjusts_indices() {
        let mut state = app_with_workspaces(&["a", "b", "c"]);
        state.selected = 1;
        state.active = Some(1);

        state.close_selected_workspace();

        assert_eq!(state.workspaces.len(), 2);
        assert_eq!(state.selected, 1);
        assert_eq!(state.active, Some(1));
        assert_eq!(state.workspaces[1].custom_name.as_deref(), Some("c"));
    }

    #[test]
    fn close_last_workspace_clears_active() {
        let mut state = app_with_workspaces(&["only"]);
        state.selected = 0;
        state.close_selected_workspace();

        assert!(state.workspaces.is_empty());
        assert_eq!(state.active, None);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn close_workspace_at_end_adjusts_selected() {
        let mut state = app_with_workspaces(&["a", "b"]);
        state.selected = 1;
        state.active = Some(1);

        state.close_selected_workspace();

        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.selected, 0);
        assert_eq!(state.active, Some(0));
    }

    #[test]
    fn close_non_focused_workspace_keeps_focus() {
        let mut state = app_with_workspaces(&["a", "b", "c"]);
        state.selected = 1;
        state.active = Some(0);

        state.close_selected_workspace();

        assert_eq!(state.workspaces.len(), 2);
        assert_eq!(state.workspaces[0].display_name(), "a");
        assert_eq!(state.workspaces[1].display_name(), "c");
        assert_eq!(state.selected, 0);
        assert_eq!(state.active, Some(0));
        state.assert_invariants_for_test();
    }

    #[test]
    fn pane_died_last_pane_removes_workspace() {
        let mut state = app_with_workspaces(&["a", "b"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");

        state.handle_pane_died(pane_id);

        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].custom_name.as_deref(), Some("b"));
        state.assert_invariants_for_test();
    }

    #[test]
    fn pane_died_closing_a_workspace_tears_it_down_like_an_explicit_close() {
        let mut state = app_with_workspaces(&["a", "dying", "c"]);
        state.active = Some(2);
        state.selected = 0;
        let pane_id = state.workspaces[1].tabs[0].root_pane;
        let terminal_id = state
            .terminal_id_for_pane(1, pane_id)
            .expect("test precondition");
        state
            .public_pane_id_aliases
            .insert("wOLD:p9".into(), pane_id);
        state.direct_attach_resize_locks.insert(terminal_id.clone());
        state.session_dirty = false;

        state.handle_pane_died(pane_id);

        assert_eq!(state.workspaces.len(), 2);
        assert_eq!(
            state.workspaces[state.active.expect("active")].display_name(),
            "c"
        );
        assert_eq!(state.workspaces[state.selected].display_name(), "a");
        assert!(!state.terminals.contains_key(&terminal_id));
        assert!(state.terminal_runtime_shutdowns.contains(&terminal_id));
        assert!(!state.direct_attach_resize_locks.contains(&terminal_id));
        assert!(state.public_pane_id_aliases.is_empty());
        assert!(state.session_dirty);
        state.assert_invariants_for_test();
    }

    #[test]
    fn pane_died_last_workspace_enters_navigate() {
        let mut state = app_with_workspaces(&["only"]);
        state.mode = Mode::Terminal;
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");

        state.handle_pane_died(pane_id);

        assert!(state.workspaces.is_empty());
        assert_eq!(state.mode, Mode::Navigate);
        state.assert_invariants_for_test();
    }

    #[test]
    fn pane_died_multi_pane_keeps_workspace() {
        let mut state = app_with_workspaces(&["test"]);
        let second_id = state.workspaces[0].test_split(Direction::Horizontal);
        state.ensure_test_terminals();

        state.handle_pane_died(second_id);

        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].panes.len(), 1);
        state.assert_invariants_for_test();
    }

    #[test]
    fn pane_died_unknown_pane_is_noop() {
        let mut state = app_with_workspaces(&["test"]);
        let fake_id = PaneId::from_raw(9999);

        state.handle_pane_died(fake_id);

        assert_eq!(state.workspaces.len(), 1);
        state.assert_invariants_for_test();
    }
    #[test]
    fn state_changed_updates_pane() {
        let mut state = app_with_workspaces(&["test"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");

        state.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Working,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });

        let terminal_id = state.workspaces[0]
            .panes
            .get(&pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        let terminal = state
            .terminals
            .get(&terminal_id)
            .expect("test precondition");
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(terminal.detected_agent, Some(Agent::Pi));
    }

    #[test]
    fn agent_state_sequences_track_transitions_for_waiters() {
        let mut app = app_with_workspaces(&["active", "background"]);
        let pane_id = app.workspaces[1].tabs[0].root_pane;
        let terminal_id = app.workspaces[1].panes[&pane_id]
            .attached_terminal_id
            .clone();

        for (sequence, state) in [(1, AgentState::Working), (2, AgentState::Idle)] {
            app.handle_app_event(AppEvent::StateChanged {
                pane_id,
                agent: Some(Agent::Pi),
                state,
                visible_blocker: false,
                process_exited: false,
                observed_at: Instant::now(),
            });
            assert_eq!(
                app.terminals[&terminal_id].last_agent_state_change_seq,
                Some(sequence)
            );
        }
        app.assert_invariants_for_test();
    }

    #[test]
    fn managed_launch_becomes_interactive_after_state_detection() {
        let mut app = app_with_workspaces(&["active", "background"]);
        let pane_id = app.workspaces[1].tabs[0].root_pane;
        let terminal_id = app.workspaces[1].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .begin_managed_agent(
                "worker".into(),
                Agent::Pi,
                Instant::now(),
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(60),
            );

        for state in [AgentState::Blocked, AgentState::Idle] {
            app.handle_app_event(AppEvent::StateChanged {
                pane_id,
                agent: Some(Agent::Pi),
                state,
                visible_blocker: state == AgentState::Blocked,
                process_exited: false,
                observed_at: Instant::now(),
            });
        }

        let terminal = &app.terminals[&terminal_id];
        assert!(terminal.managed_agent_interactive_ready());
        assert_eq!(terminal.state, AgentState::Idle);
    }

    #[test]
    fn agent_prompt_observation_changes_readiness_without_state_change() {
        let mut app = app_with_workspaces(&["active", "background"]);
        let pane_id = app.workspaces[1].tabs[0].root_pane;
        let terminal_id = app.workspaces[1].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .begin_managed_agent(
                "reviewer".into(),
                Agent::Codex,
                Instant::now(),
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(60),
            );
        app.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Codex),
            state: AgentState::Unknown,
            visible_blocker: false,
            process_exited: false,
            observed_at: Instant::now(),
        });
        app.handle_app_event(AppEvent::AgentPromptObserved {
            pane_id,
            agent: Agent::Codex,
            ready: true,
        });

        let terminal = &app.terminals[&terminal_id];
        assert!(terminal.managed_agent_interactive_ready());
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[test]
    fn visible_blocker_overrides_hook_working() {
        let mut state = app_with_workspaces(&["active", "background"]);
        state.active = Some(0);
        let bg_pane_id = *state.workspaces[1]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let bg_terminal_id = state.workspaces[1]
            .panes
            .get(&bg_pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();

        state.handle_app_event(AppEvent::StateChanged {
            pane_id: bg_pane_id,
            agent: Some(Agent::Codex),
            state: AgentState::Idle,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        state.handle_app_event(AppEvent::HookStateReported {
            pane_id: bg_pane_id,
            source: "shepr:codex".into(),
            agent_label: "codex".into(),
            state: AgentState::Working,
            message: None,
            seq: Some(1),
            session_ref: None,
        });
        state.handle_app_event(AppEvent::StateChanged {
            pane_id: bg_pane_id,
            agent: Some(Agent::Codex),
            state: AgentState::Blocked,
            visible_blocker: true,

            process_exited: false,
            observed_at: std::time::Instant::now(),
        });

        let terminal = state
            .terminals
            .get(&bg_terminal_id)
            .expect("test precondition");
        assert_eq!(terminal.state, AgentState::Blocked);
    }

    #[test]
    fn reserved_native_state_report_does_not_override_screen_state() {
        let mut state = app_with_workspaces(&["active"]);
        state.active = Some(0);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let terminal_id = state.workspaces[0]
            .panes
            .get(&pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();

        state.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Claude),
            state: AgentState::Working,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        state.handle_app_event(AppEvent::HookStateReported {
            pane_id,
            source: "shepr:claude".into(),
            agent_label: "claude".into(),
            state: AgentState::Blocked,
            message: None,
            seq: Some(1),
            session_ref: crate::agent_resume::AgentSessionRef::id("claude-session"),
        });
        let terminal = state
            .terminals
            .get(&terminal_id)
            .expect("test precondition");
        assert_eq!(terminal.state, AgentState::Working);
        assert!(terminal.hook_authority.is_none());
        assert!(terminal.persisted_agent_session.is_some());

        state.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Claude),
            state: AgentState::Idle,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });

        let terminal = state
            .terminals
            .get(&terminal_id)
            .expect("test precondition");
        assert_eq!(terminal.state, AgentState::Idle);
    }

    #[test]
    fn official_release_preserves_process_owned_agent_identity() {
        let mut state = app_with_workspaces(&["active"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let terminal_id = state.workspaces[0]
            .panes
            .get(&pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();

        state.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Working,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        let terminal = state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "shepr:pi".into(),
            agent: crate::agents::Agent::Pi,
            session_ref: crate::agent_resume::AgentSessionRef::path(
                std::env::current_dir()
                    .expect("test precondition")
                    .join("release-session.jsonl")
                    .display()
                    .to_string(),
            )
            .expect("test precondition"),
        });
        terminal.set_hook_authority(
            "shepr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            Some(1),
        );
        terminal.set_agent_name("reviewer".into());
        state.session_dirty = false;

        let updates = state.handle_app_event(AppEvent::HookAgentReleased {
            pane_id,
            source: "shepr:pi".into(),
            agent_label: "pi".into(),
            known_agent: Some(Agent::Pi),
            seq: Some(2),
        });

        assert!(updates.is_empty());
        let terminal = &state.terminals[&terminal_id];
        assert_eq!(terminal.state, AgentState::Working);
        assert_eq!(terminal.detected_agent, Some(Agent::Pi));
        assert_eq!(terminal.agent_name.as_deref(), Some("reviewer"));
        assert!(terminal.full_lifecycle_hook_authority_active());
        assert!(!state.session_dirty);
    }

    #[test]
    fn devin_state_report_refreshes_session_without_overriding_screen_state() {
        let mut state = app_with_workspaces(&["active"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let terminal_id = state.workspaces[0]
            .panes
            .get(&pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();

        state.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Devin),
            state: AgentState::Idle,
            visible_blocker: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        state.handle_app_event(AppEvent::HookStateReported {
            pane_id,
            source: "shepr:devin".into(),
            agent_label: "devin".into(),
            state: AgentState::Working,
            message: None,
            seq: Some(1),
            session_ref: crate::agent_resume::AgentSessionRef::id("devin-session"),
        });

        let terminal = state
            .terminals
            .get(&terminal_id)
            .expect("test precondition");
        assert_eq!(terminal.state, AgentState::Idle);
        assert!(terminal.hook_authority.is_none());
        assert!(terminal.persisted_agent_session.is_some());
    }

    #[test]
    fn hidden_custom_session_ref_only_update_marks_session_dirty_without_visible_update() {
        let mut state = app_with_workspaces(&["active"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let test_dir = std::env::current_dir().expect("test precondition");
        let first_session = test_dir.join("one.jsonl").display().to_string();
        let second_session = test_dir.join("two.jsonl").display().to_string();

        let first_updates = state.handle_app_event(AppEvent::HookStateReported {
            pane_id,
            source: "custom:pi".into(),
            agent_label: "pi".into(),
            state: AgentState::Working,
            message: None,
            seq: Some(20),
            session_ref: crate::agent_resume::AgentSessionRef::path(first_session),
        });
        assert_eq!(first_updates.len(), 1);
        state.session_dirty = false;

        let second_updates = state.handle_app_event(AppEvent::HookStateReported {
            pane_id,
            source: "custom:pi".into(),
            agent_label: "pi".into(),
            state: AgentState::Working,
            message: None,
            seq: Some(21),
            session_ref: crate::agent_resume::AgentSessionRef::path(second_session),
        });

        assert!(second_updates.is_empty());
        assert!(state.session_dirty);
    }

    #[test]
    fn custom_release_clears_report_owned_agent() {
        let mut state = app_with_workspaces(&["active"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let terminal_id = state.workspaces[0]
            .pane_state(pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_hook_authority(
                "custom:agent".into(),
                "custom-agent".into(),
                AgentState::Working,
                None,
                Some(1),
            );

        state.handle_app_event(AppEvent::HookAgentReleased {
            pane_id,
            source: "custom:agent".into(),
            agent_label: "custom-agent".into(),
            known_agent: None,
            seq: Some(2),
        });

        let terminal = &state.terminals[&terminal_id];
        assert!(terminal.hook_authority.is_none());
        assert_eq!(terminal.state, AgentState::Unknown);
    }

    #[test]
    fn terminal_cwd_report_updates_terminal_cwd_and_marks_session_dirty() {
        let mut state = app_with_workspaces(&["active"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let terminal_id = state.workspaces[0]
            .pane_state(pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        // Deliberately a path that does not exist: the reporting thread
        // validates the directory, and the main loop must not stat it again.
        let cwd = std::path::PathBuf::from(format!(
            "/shepr-cwd-report-test-{}/does-not-exist",
            std::process::id()
        ));
        state.session_dirty = false;

        let updates = state.handle_app_event(AppEvent::TerminalCwdReported {
            pane_id,
            cwd: cwd.clone(),
        });

        assert!(updates.is_empty());
        assert_eq!(
            state
                .terminals
                .get(&terminal_id)
                .expect("test precondition")
                .cwd,
            cwd
        );
        assert!(state.session_dirty);
    }

    #[test]
    fn relative_terminal_cwd_report_is_ignored() {
        let mut state = app_with_workspaces(&["active"]);
        let pane_id = *state.workspaces[0]
            .panes
            .keys()
            .next()
            .expect("test precondition");
        let terminal_id = state.workspaces[0]
            .pane_state(pane_id)
            .expect("test precondition")
            .attached_terminal_id
            .clone();
        let before = state.terminals[&terminal_id].cwd.clone();
        state.session_dirty = false;

        state.handle_app_event(AppEvent::TerminalCwdReported {
            pane_id,
            cwd: std::path::PathBuf::from("relative/dir"),
        });

        assert_eq!(state.terminals[&terminal_id].cwd, before);
        assert!(!state.session_dirty);
    }

    #[test]
    fn metadata_expiry_state_change_bumps_state_sequence() {
        let mut state = app_with_workspaces(&["active", "background"]);
        state.active = Some(0);
        let pane_id = state.workspaces[1].tabs[0].root_pane;
        let terminal_id = state
            .terminal_id_for_pane(1, pane_id)
            .expect("test precondition");
        let before_report = Instant::now();
        {
            let terminal = state
                .terminals
                .get_mut(&terminal_id)
                .expect("test precondition");
            terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
            terminal.set_agent_metadata(crate::terminal::AgentMetadataReport {
                source: "custom:status".into(),
                agent_label: None,
                applies_to_source: None,
                title: Some("temporary".into()),
                display_agent: None,
                state_labels: std::collections::HashMap::new(),
                clear_title: false,
                clear_display_agent: false,
                clear_state_labels: false,
                ttl: Some(std::time::Duration::from_millis(1)),
                seq: None,
            });
            // Leave the effective state stale so the expiry's recompute is
            // what moves it, the way a time-dependent authority change would.
            terminal.state = AgentState::Working;
        }
        let seq_before = state.next_agent_state_change_seq;

        let updates = state.expire_agent_metadata_at(
            before_report,
            Instant::now() + std::time::Duration::from_secs(1),
        );

        let update = updates.first().expect("expiry publishes a state update");
        assert_eq!(update.previous_state, AgentState::Working);
        assert_eq!(update.state, AgentState::Idle);
        assert_eq!(state.next_agent_state_change_seq, seq_before + 1);
        let terminal = &state.terminals[&terminal_id];
        assert_eq!(
            terminal.last_agent_state_change_seq,
            Some(state.next_agent_state_change_seq)
        );
    }

    #[test]
    fn toggle_zoom_works() {
        let mut state = app_with_workspaces(&["test"]);
        state.workspaces[0].test_split(Direction::Horizontal);

        assert!(!state.workspaces[0].zoomed);
        state.toggle_zoom();
        assert!(state.workspaces[0].zoomed);
        state.toggle_zoom();
        assert!(!state.workspaces[0].zoomed);
    }

    #[test]
    fn toggle_zoom_single_pane_noop() {
        let mut state = app_with_workspaces(&["test"]);
        state.toggle_zoom();
        assert!(!state.workspaces[0].zoomed);
    }

    #[test]
    fn navigate_pane_changes_focus_while_zoomed() {
        let mut state = app_with_workspaces(&["test"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let right = state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].layout.focus_pane(root);
        state.workspaces[0].zoomed = true;
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            ratatui::layout::Rect::new(0, 0, 100, 20),
        );

        assert_eq!(state.view.pane_infos.len(), 1);
        assert_eq!(state.view.pane_infos[0].id, root);

        state.navigate_pane(NavDirection::Right);
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            ratatui::layout::Rect::new(0, 0, 100, 20),
        );

        assert!(state.workspaces[0].zoomed);
        assert_eq!(state.workspaces[0].focused_pane_id(), Some(right));
        assert_eq!(state.view.pane_infos.len(), 1);
        assert_eq!(state.view.pane_infos[0].id, right);
        assert!(state.view.pane_infos[0].inner_rect.x > state.view.pane_infos[0].rect.x);
    }

    #[test]
    fn swap_pane_direction_preserves_focus_and_swaps_layout_cells() {
        let mut state = app_with_workspaces(&["test"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let right = state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].layout.focus_pane(root);
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            ratatui::layout::Rect::new(0, 0, 100, 20),
        );
        let before_root_rect = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition")
            .rect;
        let before_right_rect = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == right)
            .expect("test precondition")
            .rect;

        assert!(state.swap_pane(NavDirection::Right));
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            ratatui::layout::Rect::new(0, 0, 100, 20),
        );

        assert_eq!(state.workspaces[0].focused_pane_id(), Some(root));
        assert_eq!(
            state
                .view
                .pane_infos
                .iter()
                .find(|info| info.id == root)
                .expect("test precondition")
                .rect,
            before_right_rect
        );
        assert_eq!(
            state
                .view
                .pane_infos
                .iter()
                .find(|info| info.id == right)
                .expect("test precondition")
                .rect,
            before_root_rect
        );
    }

    #[test]
    fn swap_pane_direction_stays_zoomed_and_mutates_hidden_layout() {
        let mut state = app_with_workspaces(&["test"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let right = state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].layout.focus_pane(root);
        state.workspaces[0].zoomed = true;
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            ratatui::layout::Rect::new(0, 0, 100, 20),
        );

        assert!(state.swap_pane(NavDirection::Right));
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            ratatui::layout::Rect::new(0, 0, 100, 20),
        );

        assert!(state.workspaces[0].zoomed);
        assert_eq!(state.workspaces[0].focused_pane_id(), Some(root));
        assert_eq!(state.view.pane_infos.len(), 1);
        assert_eq!(state.view.pane_infos[0].id, root);

        state.workspaces[0].zoomed = false;
        crate::ui::compute_view_with_runtime_registry(
            &mut state,
            &crate::terminal::TerminalRuntimeRegistry::new(),
            ratatui::layout::Rect::new(0, 0, 100, 20),
        );
        let root_rect = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition")
            .rect;
        let right_rect = state
            .view
            .pane_infos
            .iter()
            .find(|info| info.id == right)
            .expect("test precondition")
            .rect;

        assert!(root_rect.x > right_rect.x);
    }

    #[test]
    fn close_pane_removes_from_workspace() {
        let mut state = app_with_workspaces(&["test"]);
        let closed = state.workspaces[0].test_split(Direction::Horizontal);
        state.ensure_test_terminals();
        assert_eq!(state.workspaces[0].panes.len(), 2);
        let _ = closed;
        state.close_pane();
        assert_eq!(state.workspaces[0].panes.len(), 1);
        state.assert_invariants_for_test();
    }

    #[test]
    fn pane_process_exit_publish_marks_agent_idle_before_pane_removal() {
        let mut state = app_with_workspaces(&["active", "background"]);
        state.active = Some(1);
        state.ensure_test_terminals();
        let pane_id = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state
            .terminal_id_for_pane(0, pane_id)
            .expect("test precondition");
        state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_detected_state(Some(Agent::Pi), AgentState::Working);
        assert_eq!(
            state
                .terminals
                .get(&terminal_id)
                .expect("test precondition")
                .state,
            AgentState::Working
        );

        let update = state
            .publish_pane_process_exit_if_agent(pane_id)
            .expect("process exit update");

        assert_eq!(update.workspace_id, state.workspaces[0].id);
        assert_eq!(update.previous_state, AgentState::Working);
        assert_eq!(update.state, AgentState::Idle);
        assert_eq!(update.agent_label.as_deref(), Some("pi"));
        assert_eq!(update.known_agent, Some(Agent::Pi));
        assert!(update.agent_released);
    }

    #[test]
    fn close_pane_removes_unattached_terminal_state() {
        let mut state = app_with_workspaces(&["test"]);
        let pane_id = state.workspaces[0].test_split(Direction::Horizontal);
        state.ensure_test_terminals();
        let terminal_id = state
            .terminal_id_for_pane(0, pane_id)
            .expect("test precondition");

        state.close_pane();

        assert!(!state.terminals.contains_key(&terminal_id));
        state.assert_invariants_for_test();
    }

    #[test]
    fn close_tab_removes_unattached_terminal_states() {
        let mut state = app_with_workspaces(&["test"]);
        let tab_idx = state.workspaces[0].test_add_tab(Some("logs"));
        state.ensure_test_terminals();
        state.workspaces[0].switch_tab(tab_idx);
        let pane_id = state.workspaces[0].tabs[tab_idx].root_pane;
        let terminal_id = state
            .terminal_id_for_pane(0, pane_id)
            .expect("test precondition");
        state.close_tab();

        assert!(!state.terminals.contains_key(&terminal_id));
        state.assert_invariants_for_test();
    }

    #[test]
    fn close_workspace_prunes_aliases_and_resize_locks_of_its_panes() {
        let mut state = app_with_workspaces(&["closing", "kept"]);
        let closing_pane = state.workspaces[0].tabs[0].root_pane;
        let kept_pane = state.workspaces[1].tabs[0].root_pane;
        let closing_terminal = state
            .terminal_id_for_pane(0, closing_pane)
            .expect("test precondition");
        let kept_terminal = state
            .terminal_id_for_pane(1, kept_pane)
            .expect("test precondition");
        state
            .public_pane_id_aliases
            .insert("wOLD:p1".into(), closing_pane);
        state
            .public_pane_id_aliases
            .insert("wOLD:p2".into(), kept_pane);
        state
            .direct_attach_resize_locks
            .insert(closing_terminal.clone());
        state
            .direct_attach_resize_locks
            .insert(kept_terminal.clone());

        state.close_workspace_at(0);

        assert!(!state.public_pane_id_aliases.contains_key("wOLD:p1"));
        assert_eq!(
            state.public_pane_id_aliases.get("wOLD:p2"),
            Some(&kept_pane)
        );
        assert!(!state.direct_attach_resize_locks.contains(&closing_terminal));
        assert!(state.direct_attach_resize_locks.contains(&kept_terminal));
        state.assert_invariants_for_test();
    }

    #[test]
    fn closing_the_last_workspace_leaves_terminal_mode() {
        let mut state = app_with_workspaces(&["only"]);
        assert_eq!(state.mode, Mode::Terminal);

        state.close_workspace_at(0);

        assert!(state.workspaces.is_empty());
        assert_eq!(state.mode, Mode::Navigate);
        state.assert_invariants_for_test();
    }

    #[test]
    fn close_workspace_at_keeps_the_sidebar_selection_and_focus() {
        let mut state = app_with_workspaces(&["a", "b", "c", "d"]);
        let active_id = state.workspaces[3].id.clone();
        let selected_id = state.workspaces[2].id.clone();
        state.active = Some(3);
        state.selected = 2;

        state.close_workspace_at(0);

        assert_eq!(
            state.workspaces[state.active.expect("active")].id,
            active_id
        );
        assert_eq!(state.workspaces[state.selected].id, selected_id);
        state.assert_invariants_for_test();
    }

    #[test]
    fn close_workspace_at_out_of_range_is_a_noop() {
        let mut state = app_with_workspaces(&["a"]);
        state.session_dirty = false;

        state.close_workspace_at(3);

        assert_eq!(state.workspaces.len(), 1);
        assert!(!state.session_dirty);
    }

    #[test]
    fn close_workspace_removes_unattached_terminal_states() {
        let mut state = app_with_workspaces(&["one", "two"]);
        let pane_id = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state
            .terminal_id_for_pane(0, pane_id)
            .expect("test precondition");
        let _ = pane_id;
        state.close_selected_workspace();

        assert!(!state.terminals.contains_key(&terminal_id));
        state.assert_invariants_for_test();
    }

    #[test]
    fn close_tab_closes_active_workspace_not_selected_workspace() {
        let mut state = app_with_workspaces(&["selected", "active"]);
        let active_terminal_id = state
            .terminal_id_for_pane(1, state.workspaces[1].tabs[0].root_pane)
            .expect("test precondition");
        state.active = Some(1);
        state.selected = 0;

        state.close_tab();

        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].display_name(), "selected");
        assert!(!state.terminals.contains_key(&active_terminal_id));
        state.assert_invariants_for_test();
    }

    #[test]
    fn close_pane_last_pane_closes_active_workspace_not_selected_workspace() {
        let mut state = app_with_workspaces(&["selected", "active"]);
        let active_terminal_id = state
            .terminal_id_for_pane(1, state.workspaces[1].tabs[0].root_pane)
            .expect("test precondition");
        state.active = Some(1);
        state.selected = 0;

        state.close_pane();

        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].display_name(), "selected");
        assert!(!state.terminals.contains_key(&active_terminal_id));
        state.assert_invariants_for_test();
    }
}
