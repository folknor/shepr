use crate::shell::state::ClientShellAction;

use crate::endpoint::ClientEndpointId;
use crate::shell::navigation::location::Location;
use crate::shell::state::{ClientShellInput, ClientShellState, ClientWorkspacePress};
use crate::shell::{EndpointNotice, EndpointNoticeKind};

impl ClientShellState {
    pub(in crate::shell) fn active_endpoint_workspace_at(
        &self,
        point: (u16, u16),
    ) -> Option<shepr_protocol::WorkspaceId> {
        self.hits
            .workspaces
            .iter()
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
            && self.snapshot.as_deref().is_some_and(|snapshot| {
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
            .hits
            .machines
            .iter()
            .find(|hit| crate::shell::input::hit_test::contains(hit.rect, point))
        else {
            return false;
        };
        let endpoint_id = hit.location.endpoint.clone();
        let collapse_toggle = crate::shell::input::hit_test::contains(hit.collapse_toggle, point);
        if collapse_toggle {
            if !self.collapsed_endpoints.remove(&endpoint_id) {
                self.collapsed_endpoints.insert(endpoint_id.clone());
            }
            outcome.repaint = true;
        } else if endpoint_id == *self.endpoints.presented() {
            if !self.collapsed_endpoints.remove(&endpoint_id) {
                self.collapsed_endpoints.insert(endpoint_id.clone());
            }
            // Selecting the shown endpoint cancels a move in progress.
            self.activate_endpoint(endpoint_id, outcome);
            outcome.repaint = true;
        } else if self.endpoint_can_select(&endpoint_id) {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: None,
            });
        } else {
            self.receive_endpoint_unavailable(&EndpointNotice::new(
                endpoint_id,
                EndpointNoticeKind::NotReady,
            ));
            outcome.repaint = true;
        }
        true
    }

    pub(in crate::shell) fn handle_endpoint_agent_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(location) = self
            .hits
            .agent_hits
            .iter()
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
                .snapshot
                .as_deref()
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
            let agents = self.agent_panel_model.targets();
            if agents.is_empty() {
                return true;
            }
            let focused = self
                .snapshot
                .as_deref()
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
            let target_endpoint_id = target.endpoint.clone();
            let Some(target_pane_id) = target.pane_id() else {
                return true;
            };
            if self.focus_or_activate(target.clone(), outcome) {
                if target_endpoint_id == *self.endpoints.presented() {
                    self.reveal_endpoint_agent(
                        &target_endpoint_id,
                        &target_pane_id,
                        self.hits.agent_body.height,
                    );
                } else {
                    self.pending_agent_reveal = Some(target);
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
        self.pending_agent_reveal = None;
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
        outcome.actions.push(ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: None,
        });
        true
    }

    pub(in crate::shell) fn focus_or_activate(
        &mut self,
        location: Location,
        outcome: &mut ClientShellInput,
    ) -> bool {
        self.pending_workspace_highlight = None;
        self.pending_agent_reveal = None;
        let target = location.focus_target();
        let endpoint_id = location.endpoint;
        if !self.endpoint_can_select(&endpoint_id) {
            self.receive_endpoint_unavailable(&EndpointNotice::new(
                endpoint_id,
                EndpointNoticeKind::NotReady,
            ));
            outcome.repaint = true;
            return false;
        }
        // The runtime resolves every explicit pick against the shown endpoint: focus it,
        // prepare an unavailable endpoint, or retarget a move already in progress.
        outcome.actions.push(ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::shell::navigation::location::Location;
    use crate::shell::state::ClientShellAction;
    use crate::shell::state::ClientShellConfig;
    use ratatui::layout::Rect;

    use crate::endpoint::ClientEndpointId;
    use crate::shell::endpoints::MachineHit;
    use crate::shell::state::{ClientShellInput, ClientShellState};

    #[test]
    fn displayed_machine_body_submits_a_targetless_selection() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        state.hits.machines.push(MachineHit {
            rect: Rect::new(0, 0, 10, 1),
            status_badge: Rect::default(),
            collapse_toggle: Rect::new(0, 0, 1, 1),
            location: Location::machine(ClientEndpointId::Local),
        });
        let mut outcome = ClientShellInput::default();

        assert!(state.handle_endpoint_machine_click((5, 0), &mut outcome));
        assert!(matches!(
            outcome.actions.as_slice(),
            [ClientShellAction::ActivateEndpoint {
                endpoint_id: ClientEndpointId::Local,
                target: None,
            }]
        ));
        assert!(state.collapsed_endpoints.contains(&ClientEndpointId::Local));
    }
}
