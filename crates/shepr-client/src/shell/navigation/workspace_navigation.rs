use crate::endpoint::ClientEndpointId;
use crate::limits::WORKSPACE_HIGHLIGHT_TIMEOUT;
use crate::shell::endpoints::{ClientShellEndpoint, MachineAction, MachineState};
use crate::shell::ledger::Ticket;
use crate::shell::navigation::location::{Location, LocationTarget, PinnedLocation};
use crate::shell::state::{ClientShellAction, ClientShellMode};
use crate::shell::state::{ClientShellInput, ClientShellState, Repaint};
use crate::shell::{EndpointNotice, EndpointNoticeKind};

/// Display-only continuity while a direct focus request awaits its authoritative snapshot.
pub(in crate::shell) struct PendingWorkspaceHighlight {
    pub(in crate::shell) target: PinnedLocation,
    ticket: Ticket,
    expires_at: std::time::Instant,
}

impl PendingWorkspaceHighlight {
    /// Clears `slot` when it holds `ticket`.
    pub(in crate::shell) fn release(slot: &mut Option<Self>, ticket: Ticket) -> Repaint {
        if slot.as_ref().is_some_and(|held| held.ticket == ticket) {
            *slot = None;
            Repaint::Needed
        } else {
            Repaint::Unchanged
        }
    }
}

impl PinnedLocation {
    pub(in crate::shell) fn matches_workspace(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        self.matches(endpoint_id, LocationTarget::Workspace(*workspace_id))
    }

    pub(in crate::shell) fn matches_pane(
        &self,
        endpoint_id: &ClientEndpointId,
        pane_id: &shepr_protocol::PublicPaneId,
    ) -> bool {
        self.matches(endpoint_id, LocationTarget::Pane(*pane_id))
    }
}

pub(super) fn workspace_navigation_targets(
    endpoints: &[ClientShellEndpoint],
) -> Vec<PinnedLocation> {
    let mut targets = Vec::new();
    for endpoint in endpoints {
        if endpoint.state.stale() {
            continue;
        }
        let Some(snapshot) = endpoint.snapshot() else {
            continue;
        };
        let Some(generation) = endpoint.snapshot_generation() else {
            continue;
        };
        for workspace in &snapshot.workspaces {
            targets.push(PinnedLocation::new(
                Location::workspace(endpoint.endpoint_id.clone(), workspace.workspace_id),
                snapshot.boot_id.clone(),
                generation,
            ));
        }
    }
    targets
}

impl ClientShellState {
    pub(super) fn keep_workspace_highlight_until_snapshot(
        &mut self,
        target: PinnedLocation,
        ticket: Ticket,
        now: std::time::Instant,
    ) {
        self.pending_workspace_highlight = Some(PendingWorkspaceHighlight {
            target,
            ticket,
            expires_at: now + WORKSPACE_HIGHLIGHT_TIMEOUT,
        });
        self.reconcile_pending_workspace_highlight();
    }

    pub(in crate::shell) fn tick_workspace_highlight(&mut self, now: std::time::Instant) -> bool {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|pending| now >= pending.expires_at)
        {
            self.pending_workspace_highlight = None;
            return true;
        }
        false
    }

    pub(in crate::shell) fn workspace_highlight_deadline(&self) -> Option<std::time::Instant> {
        self.pending_workspace_highlight
            .as_ref()
            .map(|pending| pending.expires_at)
    }

    pub(in crate::shell) fn reconcile_pending_workspace_highlight(&mut self) {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|pending| {
                pending.target.location.endpoint != *self.endpoints.presented()
                    || !self.navigation_target_valid(&pending.target)
                    || self.endpoints.active.snapshot().is_some_and(|snapshot| {
                        pending
                            .target
                            .location
                            .workspace_id()
                            .is_some_and(|workspace_id| {
                                snapshot.focused_workspace_id.as_ref() == Some(&workspace_id)
                            })
                    })
            })
        {
            self.pending_workspace_highlight = None;
        }
    }

    pub(in crate::shell) fn navigation_target(
        &self,
        endpoint_id: &ClientEndpointId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<PinnedLocation> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint_id)?;
        let snapshot = endpoint.snapshot()?;
        let generation = endpoint.snapshot_generation()?;
        Some(PinnedLocation::new(
            Location::workspace(endpoint_id.clone(), *workspace_id),
            snapshot.boot_id.clone(),
            generation,
        ))
    }

    pub(in crate::shell) fn focused_navigation_target(&self) -> Option<PinnedLocation> {
        let workspace_id = self
            .endpoints
            .active
            .snapshot()?
            .focused_workspace_id
            .as_ref()?;
        self.navigation_target(self.endpoints.presented(), workspace_id)
    }

    /// Whether `target` still names something navigate mode can act on: a workspace, or
    /// an agent's pane, of the snapshot it was pinned from on a usable endpoint, or a
    /// configured machine's entry while it offers its Connect or Restart.
    pub(in crate::shell) fn navigation_target_valid(&self, target: &PinnedLocation) -> bool {
        if target.is_machine_entry() {
            return self
                .machine_entry_action(&target.location.endpoint)
                .is_some();
        }
        self.endpoints.iter().any(|endpoint| {
            endpoint.endpoint_id == target.location.endpoint
                && endpoint.state.usable()
                && endpoint.snapshot_generation().is_some()
                && endpoint.snapshot_generation() == target.generation()
                && endpoint.snapshot().is_some_and(|snapshot| {
                    Some(&snapshot.boot_id) == target.boot_id()
                        && match target.location.target {
                            LocationTarget::Workspace(workspace_id) => snapshot
                                .workspaces
                                .iter()
                                .any(|workspace| workspace.workspace_id == workspace_id),
                            LocationTarget::Pane(pane_id) => {
                                snapshot.agents.iter().any(|agent| agent.pane_id == pane_id)
                            }
                            LocationTarget::Machine => false,
                        }
                })
        })
    }

    /// What activating `endpoint_id`'s machine entry would do, while it shows one that
    /// offers an action.
    pub(in crate::shell) fn machine_entry_action(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<MachineAction> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(ClientShellEndpoint::machine_entry)
            .and_then(|entry| entry.state.action())
    }

    /// The workspaces and machine entries navigate mode steps through, in sidebar
    /// order: each usable endpoint's workspaces, and in place of a configured
    /// machine's workspaces its entry while that offers Connect or Restart.
    fn navigate_list_targets(&self) -> Vec<PinnedLocation> {
        self.endpoints
            .iter()
            .flat_map(|endpoint| {
                if endpoint
                    .machine_entry()
                    .is_some_and(|entry| entry.state.action().is_some())
                {
                    vec![PinnedLocation::machine_entry(endpoint.endpoint_id.clone())]
                } else {
                    workspace_navigation_targets(std::slice::from_ref(endpoint))
                }
            })
            .collect()
    }

    /// Keeps the navigate selection on a machine entry only while that entry offers
    /// its action. One whose machine connected, or whose action went away, hands the
    /// selection to the first entry navigate mode can step to.
    pub(in crate::shell) fn reconcile_navigate_machine_entry(&mut self) {
        let Some(selected) = self
            .mode
            .preview()
            .filter(|selected| selected.is_machine_entry())
        else {
            return;
        };
        if self.navigation_target_valid(selected) {
            return;
        }
        let next = self.navigate_list_targets().into_iter().next();
        if let Some(next) = next.as_ref() {
            self.reveal_navigate_selection(next);
        }
        self.mode.set_preview(next);
    }

    /// Activates a configured machine's entry, as Enter on it in navigate mode or a
    /// click on it (or on its machine row) does: Connect starts the machine's server
    /// and attaches; Restart asks first. False when the entry offers no action.
    pub(in crate::shell) fn activate_machine_entry(
        &mut self,
        endpoint_id: &ClientEndpointId,
        outcome: &mut ClientShellInput,
    ) -> bool {
        match self.machine_entry_action(endpoint_id) {
            Some(MachineAction::Connect) => {
                self.set_machine_state(endpoint_id, MachineState::Starting);
                outcome
                    .actions
                    .push(ClientShellAction::ConnectMachine(endpoint_id.clone()));
            }
            Some(MachineAction::Restart) => self.open_confirm_restart_overlay(endpoint_id),
            None => return false,
        }
        outcome.repaint = true;
        true
    }

    /// The agents navigate mode can select, after the workspaces, in the agent panel's
    /// display order: the rows of usable machines, as many as the sidebar on screen can
    /// show (`ShellView::agent_navigation_limit`).
    fn agent_navigation_targets(&self) -> Vec<PinnedLocation> {
        let rows = &self.endpoints.agent_panel_model.rows;
        let limit = self
            .view()
            .and_then(crate::shell::view::ShellView::agent_navigation_limit)
            .unwrap_or(rows.len());
        rows.iter()
            .take(limit)
            .filter(|row| !row.stale)
            .filter_map(|row| {
                let endpoint = self
                    .endpoints
                    .iter()
                    .find(|endpoint| endpoint.endpoint_id == row.endpoint_id)?;
                Some(PinnedLocation::new(
                    Location::pane(row.endpoint_id.clone(), row.agent.pane_id),
                    endpoint.snapshot()?.boot_id.clone(),
                    endpoint.snapshot_generation()?,
                ))
            })
            .collect()
    }

    /// The Navigate selection's place among the selectable agents, when it is on one.
    /// Read before the agent panel is rebuilt, for `reconcile_navigate_agent`.
    pub(in crate::shell) fn navigate_agent_index(&self) -> Option<usize> {
        let selected = self
            .mode
            .preview()
            .filter(|selected| selected.location.pane_id().is_some())?;
        self.agent_navigation_targets()
            .iter()
            .position(|target| target.location == selected.location)
    }

    /// Keeps a Navigate selection on an agent across a rebuild of the agent panel, given
    /// its place `previous` before the rebuild. The selection stays on its agent while that
    /// agent is still selectable on the same boot, pinned to the snapshot that now presents
    /// it. An agent that went away, or whose machine went stale, hands the selection to the
    /// agent now at its place (the last one when the list got shorter), or with no agents
    /// left to the last workspace, where moving up from the first agent leads.
    pub(in crate::shell) fn reconcile_navigate_agent(&mut self, previous: Option<usize>) {
        let Some(selected) = self
            .mode
            .preview()
            .filter(|selected| selected.location.pane_id().is_some())
            .cloned()
        else {
            return;
        };
        let mut agents = self.agent_navigation_targets();
        let kept = agents.iter().position(|target| {
            target.location == selected.location && target.boot_id() == selected.boot_id()
        });
        let next = match kept {
            Some(index) => Some(agents.swap_remove(index)),
            None if agents.is_empty() => workspace_navigation_targets(&self.endpoints).pop(),
            None => {
                let index = previous.unwrap_or(0).min(agents.len().saturating_sub(1));
                Some(agents.swap_remove(index))
            }
        };
        if let Some(next) = next.as_ref()
            && next.location != selected.location
        {
            self.reveal_navigate_selection(next);
        }
        self.mode.set_preview(next);
    }

    /// Brings a new Navigate selection into view in the list that draws it.
    fn reveal_navigate_selection(&mut self, target: &PinnedLocation) {
        match target.location.target {
            LocationTarget::Pane(_) => self.sidebar_scroll.reveal_agent(target.location.clone()),
            LocationTarget::Workspace(_) | LocationTarget::Machine => {
                self.sidebar_scroll.reveal_selected_workspace();
            }
        }
    }

    pub(in crate::shell) fn workspace_preview_action_blocked(&self) -> bool {
        self.mode.preview().is_some_and(|target| {
            target.location.endpoint != *self.endpoints.presented()
                || !self.navigation_target_valid(target)
        })
    }

    /// Moves the Navigate selection `delta` steps through one list: every workspace in
    /// sidebar order (a configured machine's Connect or Restart entry in place of its
    /// workspaces), then every selectable agent in the agent panel's order, wrapping at
    /// both ends. Moving only highlights; Enter acts.
    pub(in crate::shell) fn move_navigate_selection(&mut self, delta: isize) {
        let mut targets = self.navigate_list_targets();
        targets.extend(self.agent_navigation_targets());
        if targets.is_empty() {
            return;
        }
        let current = self
            .mode
            .preview()
            .and_then(|selected| targets.iter().position(|target| target == selected));
        let Some(next) = crate::shell::navigation::aggregate_navigation::cycle_index(
            targets.len(),
            current,
            delta,
        ) else {
            return;
        };
        let target = targets.swap_remove(next);
        self.reveal_navigate_selection(&target);
        self.mode.set_preview(Some(target));
    }

    /// Enter in navigate mode: switches to the selected workspace, or to the selected
    /// agent's machine and workspace with its pane focused, or activates the selected
    /// machine entry (Connect, or Restart after its question), the way a click on the
    /// entry does, and leaves navigate mode.
    pub(in crate::shell) fn accept_navigate_selection(&mut self, outcome: &mut ClientShellInput) {
        let Some(target) = self.mode.preview().cloned() else {
            self.mode.set(self.copy_or_terminal_mode());
            outcome.repaint = true;
            return;
        };
        if target.is_machine_entry() {
            // Activated while still in navigate mode, so a Restart question that is
            // cancelled returns to it.
            let activated = self.activate_machine_entry(&target.location.endpoint, outcome);
            self.mode.set(ClientShellMode::Terminal);
            if !activated {
                self.receive_endpoint_unavailable(&EndpointNotice::new(
                    target.location.endpoint,
                    EndpointNoticeKind::NotReady,
                ));
            }
            outcome.repaint = true;
            return;
        }
        let agent = target.location.pane_id().is_some();
        if !self.navigation_target_valid(&target) {
            self.receive_endpoint_unavailable(&EndpointNotice::new(
                target.location.endpoint.clone(),
                if agent {
                    EndpointNoticeKind::AgentNoLongerAvailable
                } else {
                    EndpointNoticeKind::WorkspaceNoLongerAvailable
                },
            ));
            outcome.repaint = true;
            return;
        }
        if self.focus_or_activate(target.location.clone(), outcome) {
            if agent {
                // As the agent keybindings do: the panel keeps the focused agent in view,
                // once its machine is the one presented.
                if target.location.endpoint == *self.endpoints.presented() {
                    self.sidebar_scroll.reveal_agent(target.location);
                } else {
                    self.sidebar_scroll
                        .reveal_agent_after_activation(target.location);
                }
            }
            self.mode.set(ClientShellMode::Terminal);
        }
        outcome.repaint = true;
    }
}
