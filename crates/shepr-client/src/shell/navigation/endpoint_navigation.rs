use super::*;

impl ClientShellState {
    pub(super) fn active_endpoint_workspace_at(
        &self,
        point: (u16, u16),
    ) -> Option<shepr_protocol::WorkspaceId> {
        self.hits
            .workspaces
            .iter()
            .find(|hit| {
                hit.endpoint_id == self.active_endpoint_id && super::contains(hit.rect, point)
            })
            .map(|hit| hit.workspace_id.clone())
    }

    pub(super) fn endpoint_workspace_is_draggable(&self, press: &ClientWorkspacePress) -> bool {
        press.endpoint_id == self.active_endpoint_id
            && self.snapshot.as_deref().is_some_and(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .any(|workspace| workspace.workspace_id == press.workspace_id)
            })
    }

    pub(super) fn finish_endpoint_workspace_press(
        &mut self,
        press: ClientWorkspacePress,
        outcome: &mut ClientShellInput,
    ) {
        self.focus_or_activate(
            press.endpoint_id,
            ClientEndpointFocusTarget::Workspace(press.workspace_id),
            outcome,
        );
    }

    pub(super) fn handle_endpoint_machine_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(hit) = self
            .hits
            .machines
            .iter()
            .find(|hit| super::contains(hit.rect, point))
        else {
            return false;
        };
        let endpoint_id = hit.endpoint_id.clone();
        let collapse_toggle = super::contains(hit.collapse_toggle, point);
        if collapse_toggle {
            if !self.collapsed_endpoints.remove(&endpoint_id) {
                self.collapsed_endpoints.insert(endpoint_id.clone());
            }
            outcome.repaint = true;
        } else if endpoint_id == self.active_endpoint_id {
            if !self.collapsed_endpoints.remove(&endpoint_id) {
                self.collapsed_endpoints.insert(endpoint_id.clone());
            }
            // Selecting the shown endpoint cancels a move in progress.
            self.activate_endpoint(endpoint_id, outcome);
            outcome.repaint = true;
        } else if endpoint_id.is_local() || self.endpoint_is_online(&endpoint_id) {
            outcome.actions.push(ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target: None,
            });
        } else {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is not ready"));
            outcome.repaint = true;
        }
        true
    }

    pub(super) fn handle_endpoint_agent_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some((endpoint_id, pane_id)) = self
            .hits
            .endpoint_agents
            .iter()
            .find(|(rect, _, _)| super::contains(*rect, point))
            .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
        else {
            return false;
        };
        self.focus_or_activate(
            endpoint_id,
            ClientEndpointFocusTarget::Pane(pane_id),
            outcome,
        );
        true
    }

    pub(super) fn handle_endpoint_navigation(
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
            let workspaces = self
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.status == ClientEndpointStatus::Online)
                .flat_map(|endpoint| {
                    endpoint
                        .snapshot
                        .as_deref()
                        .map_or_else(Vec::new, |snapshot| {
                            snapshot
                                .workspaces
                                .iter()
                                .map(|workspace| {
                                    (endpoint.endpoint_id.clone(), workspace.workspace_id.clone())
                                })
                                .collect()
                        })
                })
                .collect::<Vec<_>>();
            if workspaces.is_empty() {
                return true;
            }
            let focused = self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_workspace_id.as_ref());
            let current = workspaces.iter().position(|(endpoint_id, workspace_id)| {
                endpoint_id == &self.active_endpoint_id && Some(workspace_id) == focused
            });
            let next = match (current, action) {
                (Some(index), KeybindAction::PreviousWorkspace) => {
                    (index + workspaces.len() - 1) % workspaces.len()
                }
                (Some(index), KeybindAction::NextWorkspace) => (index + 1) % workspaces.len(),
                (None, KeybindAction::PreviousWorkspace) => workspaces.len() - 1,
                (None, KeybindAction::NextWorkspace) => 0,
                _ => unreachable!("endpoint workspace navigation"),
            };
            let (endpoint_id, workspace_id) = workspaces[next].clone();
            self.focus_or_activate(
                endpoint_id,
                ClientEndpointFocusTarget::Workspace(workspace_id),
                outcome,
            );
            return true;
        }
        if matches!(
            action,
            KeybindAction::PreviousAgent | KeybindAction::NextAgent | KeybindAction::FocusAgent(_)
        ) {
            let agents = super::aggregate_navigation::displayed_agent_targets(
                &self.endpoints,
                self.config.agent_panel_sort,
            );
            if agents.is_empty() {
                return true;
            }
            let focused = self
                .snapshot
                .as_deref()
                .and_then(|snapshot| snapshot.focused_pane_id.as_deref());
            let Some(next) = super::aggregate_navigation::agent_target_index(
                &agents,
                &self.active_endpoint_id,
                focused,
                action,
            ) else {
                return true;
            };
            let target = &agents[next];
            if self.focus_or_activate(
                target.endpoint_id.clone(),
                ClientEndpointFocusTarget::Pane(target.pane_id.clone()),
                outcome,
            ) {
                if target.endpoint_id == self.active_endpoint_id {
                    self.reveal_endpoint_agent(
                        &target.endpoint_id,
                        &target.pane_id,
                        self.hits.agent_body.height,
                    );
                } else {
                    self.pending_agent_reveal =
                        Some((target.endpoint_id.clone(), target.pane_id.clone()));
                }
                outcome.repaint = true;
            }
            return true;
        }
        false
    }

    pub(super) fn activate_endpoint(
        &mut self,
        endpoint_id: ClientEndpointId,
        outcome: &mut ClientShellInput,
    ) -> bool {
        self.pending_workspace_highlight = None;
        self.pending_agent_reveal = None;
        let online = self.endpoint_is_online(&endpoint_id);
        if !online && !endpoint_id.is_local() {
            if endpoint_id != self.active_endpoint_id {
                let label = self.endpoint_label(&endpoint_id).to_owned();
                self.receive_endpoint_unavailable(format!("{label} is not ready"));
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

    pub(super) fn focus_or_activate(
        &mut self,
        endpoint_id: ClientEndpointId,
        target: ClientEndpointFocusTarget,
        outcome: &mut ClientShellInput,
    ) -> bool {
        self.pending_workspace_highlight = None;
        self.pending_agent_reveal = None;
        let online = self.endpoint_is_online(&endpoint_id);
        if !online && !endpoint_id.is_local() {
            let label = self.endpoint_label(&endpoint_id).to_owned();
            self.receive_endpoint_unavailable(format!("{label} is not ready"));
            outcome.repaint = true;
            return false;
        }
        // The runtime resolves every explicit pick against the shown endpoint: focus it,
        // prepare an unavailable endpoint, or retarget a move already in progress.
        outcome.actions.push(ClientShellAction::ActivateEndpoint {
            endpoint_id,
            target: Some(target),
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displayed_machine_body_submits_a_targetless_selection() {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(
            &shepr_config::ClientConfig::default(),
        ));
        state.hits.machines.push(MachineHit {
            rect: Rect::new(0, 0, 10, 1),
            status_badge: Rect::default(),
            collapse_toggle: Rect::new(0, 0, 1, 1),
            endpoint_id: ClientEndpointId::Local,
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
