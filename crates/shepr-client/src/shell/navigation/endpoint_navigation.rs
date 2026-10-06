use crate::shell::state::ClientShellAction;

use crate::endpoint::ClientEndpointId;
use crate::shell::input::pointer::ClientWorkspacePress;
use crate::shell::navigation::location::Location;
use crate::shell::state::{ClientShellInput, ClientShellState};
use crate::shell::{EndpointNotice, EndpointNoticeKind};

impl ClientShellState {
    pub(in crate::shell) fn active_endpoint_workspace_at(
        &self,
        point: (u16, u16),
    ) -> Option<shepr_protocol::WorkspaceId> {
        self.presentation
            .shown()
            .workspaces()
            .find(|hit| {
                hit.location.endpoint == *self.endpoints.presented()
                    && crate::shell::input::hit_test::contains(hit.rect, point)
            })
            .and_then(|hit| hit.location.workspace_id())
    }

    pub(in crate::shell) fn endpoint_workspace_is_draggable(
        &self,
        press: &ClientWorkspacePress,
    ) -> bool {
        press.location.endpoint == *self.endpoints.presented()
            && self.endpoints.active.snapshot().is_some_and(|snapshot| {
                let Some(workspace_id) = press.location.workspace_id() else {
                    return false;
                };
                snapshot
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.workspace_id == workspace_id)
            })
    }

    pub(in crate::shell) fn finish_endpoint_workspace_press(
        &mut self,
        press: ClientWorkspacePress,
        outcome: &mut ClientShellInput,
    ) {
        self.focus_or_activate(press.location, outcome);
    }

    pub(in crate::shell) fn handle_endpoint_machine_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hit) = self
            .presentation
            .shown()
            .machines()
            .find(|hit| crate::shell::input::hit_test::contains(hit.rect, point))
        else {
            return false;
        };
        let endpoint_id = hit.location.endpoint.clone();
        // A machine that offers Connect or Restart: its row acts as its entry.
        if self.activate_machine_entry(&endpoint_id, outcome) {
            return true;
        }
        if endpoint_id == *self.endpoints.presented() {
            // Selecting the shown endpoint cancels a move in progress.
            self.activate_endpoint(endpoint_id, outcome);
            outcome.repaint = true;
        } else if self.endpoint_can_select(&endpoint_id) {
            outcome
                .actions
                .push(ClientShellAction::ActivateEndpoint(Location::machine(
                    endpoint_id,
                )));
        } else {
            self.receive_endpoint_unavailable(&EndpointNotice::new(
                endpoint_id,
                EndpointNoticeKind::NotReady,
            ));
            outcome.repaint = true;
        }
        true
    }

    /// A click on a configured machine's state entry: Connect or Restart when it offers
    /// one; an entry that offers neither takes the click and does nothing.
    pub(in crate::shell) fn handle_machine_entry_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hit) = self
            .presentation
            .shown()
            .machine_entries()
            .find(|hit| crate::shell::input::hit_test::contains(hit.rect, point))
        else {
            return false;
        };
        if hit.actionable {
            let endpoint_id = hit.location.endpoint.clone();
            self.activate_machine_entry(&endpoint_id, outcome);
        }
        true
    }

    pub(in crate::shell) fn handle_endpoint_agent_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(location) = self
            .presentation
            .shown()
            .agents()
            .find(|hit| crate::shell::input::hit_test::contains(hit.rect, point))
            .map(|hit| hit.location.clone())
        else {
            return false;
        };
        self.focus_or_activate(location, outcome);
        true
    }

    pub(in crate::shell) fn handle_endpoint_navigation(
        &mut self,
        action: shepr_termio::input::KeybindAction,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use shepr_termio::input::KeybindAction;
        if !self.multi_endpoint_active() {
            return false;
        }
        if matches!(
            action,
            KeybindAction::PreviousWorkspace | KeybindAction::NextWorkspace
        ) {
            let workspaces =
                crate::shell::navigation::workspace_navigation::workspace_navigation_targets(
                    &self.endpoints,
                );
            if workspaces.is_empty() {
                return true;
            }
            let focused = self
                .endpoints
                .active
                .snapshot()
                .and_then(|snapshot| snapshot.focused_workspace_id.as_ref());
            let current = workspaces.iter().position(|target| {
                target.location.endpoint == *self.endpoints.presented()
                    && target.location.workspace_id().as_ref() == focused
            });
            let delta = if action == KeybindAction::PreviousWorkspace {
                -1
            } else {
                1
            };
            let Some(next) = crate::shell::navigation::aggregate_navigation::cycle_index(
                workspaces.len(),
                current,
                delta,
            ) else {
                return true;
            };
            let target = &workspaces[next];
            self.focus_or_activate(target.location.clone(), outcome);
            return true;
        }
        if matches!(
            action,
            KeybindAction::PreviousAgent | KeybindAction::NextAgent | KeybindAction::FocusAgent(_)
        ) {
            let agents = self.endpoints.agent_panel_model.targets();
            if agents.is_empty() {
                return true;
            }
            let focused = self
                .endpoints
                .active
                .snapshot()
                .and_then(|snapshot| snapshot.focused_pane_id.as_ref());
            let Some(next) = crate::shell::navigation::aggregate_navigation::agent_target_index(
                agents,
                self.endpoints.presented(),
                focused,
                action,
            ) else {
                return true;
            };
            let target = agents[next].clone();
            if target.pane_id().is_none() {
                return true;
            }
            if self.focus_or_activate(target.clone(), outcome) {
                if target.endpoint == *self.endpoints.presented() {
                    self.sidebar_scroll.reveal_agent(target);
                } else {
                    self.sidebar_scroll.reveal_agent_after_activation(target);
                }
                outcome.repaint = true;
            }
            return true;
        }
        false
    }

    pub(in crate::shell) fn activate_endpoint(
        &mut self,
        endpoint_id: ClientEndpointId,
        outcome: &mut ClientShellInput,
    ) -> bool {
        self.pending_workspace_highlight = None;
        self.sidebar_scroll.cancel_agent_reveal_after_activation();
        if !self.endpoint_can_select(&endpoint_id) {
            if endpoint_id != *self.endpoints.presented() {
                self.receive_endpoint_unavailable(&EndpointNotice::new(
                    endpoint_id,
                    EndpointNoticeKind::NotReady,
                ));
                outcome.repaint = true;
            }
            return false;
        }
        outcome
            .actions
            .push(ClientShellAction::ActivateEndpoint(Location::machine(
                endpoint_id,
            )));
        true
    }

    pub(in crate::shell) fn focus_or_activate(
        &mut self,
        location: Location,
        outcome: &mut ClientShellInput,
    ) -> bool {
        self.pending_workspace_highlight = None;
        self.sidebar_scroll.cancel_agent_reveal_after_activation();
        if !self.endpoint_can_select(&location.endpoint) {
            self.receive_endpoint_unavailable(&EndpointNotice::new(
                location.endpoint,
                EndpointNoticeKind::NotReady,
            ));
            outcome.repaint = true;
            return false;
        }
        // The runtime resolves every explicit pick against the shown endpoint: focus it,
        // prepare an unavailable endpoint, or retarget a move already in progress.
        outcome
            .actions
            .push(ClientShellAction::ActivateEndpoint(location));
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::shell::config::ClientShellConfig;
    use crate::shell::navigation::location::{Location, LocationTarget};
    use crate::shell::state::ClientShellAction;
    use ratatui::layout::Rect;

    use crate::endpoint::ClientEndpointId;
    use crate::shell::state::{ClientShellInput, ClientShellState};
    use crate::shell::view::{MachineHit, ShellView};

    #[test]
    fn displayed_machine_body_submits_a_targetless_selection() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        state
            .presentation
            .set_view(ShellView::with_machine_hit(MachineHit {
                rect: Rect::new(0, 0, 10, 1),
                status_badge: Rect::default(),
                location: Location::machine(ClientEndpointId::Local),
            }));
        let mut outcome = ClientShellInput::default();

        assert!(state.handle_endpoint_machine_click((0, 0), &mut outcome));
        assert!(matches!(
            outcome.actions.as_slice(),
            [ClientShellAction::ActivateEndpoint(Location {
                endpoint: ClientEndpointId::Local,
                target: LocationTarget::Machine,
            })]
        ));
    }

    fn not_running_machine() -> (ClientShellState, ClientEndpointId) {
        let machine = shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("build").expect("test label"),
            ssh: shepr_config::SshTarget::parse("build.example").expect("test target"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
        };
        let id = ClientEndpointId::Ssh(machine.label.clone());
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        state.set_machines(&[machine]);
        state.set_machine_state(&id, crate::shell::MachineState::NotRunning);
        (state, id)
    }

    /// The machine row of a machine that offers Connect acts as its entry, in the
    /// collapsed strip as in the expanded sidebar.
    #[test]
    fn a_machine_row_that_offers_connect_emits_a_request() {
        let (mut state, id) = not_running_machine();
        state
            .presentation
            .set_view(ShellView::with_machine_hit(MachineHit {
                rect: Rect::new(0, 0, 10, 1),
                status_badge: Rect::default(),
                location: Location::machine(id.clone()),
            }));
        let mut outcome = ClientShellInput::default();

        assert!(state.handle_endpoint_machine_click((3, 0), &mut outcome));
        assert!(matches!(
            outcome.actions.as_slice(),
            [ClientShellAction::ConnectMachine(connected)] if connected == &id
        ));
        assert_eq!(
            state.machine_state(&id),
            Some(crate::shell::MachineState::NotRunning)
        );
    }
}
