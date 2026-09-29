use super::*;
use crate::server::ClientId;
use crate::server::clients::ClientShellTopology;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ShellFocusTarget {
    pub(super) tab_id: shepr_protocol::PublicTabId,
    pub(super) workspace_id: shepr_protocol::WorkspaceId,
    pub(super) pane_id: shepr_core::layout::PaneId,
}

fn classify_shell_focus_transition<'a>(
    before: Option<&'a ShellFocusTarget>,
    after: Option<&'a ShellFocusTarget>,
    focused_tabs_before: &HashSet<String>,
    focused_tabs_after: &HashSet<String>,
) -> (Option<&'a ShellFocusTarget>, Option<&'a ShellFocusTarget>) {
    if before == after {
        return (None, None);
    }
    if before.map(|target| target.tab_id.as_str()) == after.map(|target| target.tab_id.as_str()) {
        return match (before, after) {
            (Some(before), Some(after)) if focused_tabs_after.contains(after.tab_id.as_str()) => {
                (Some(before), Some(after))
            }
            _ => (None, None),
        };
    }
    let lost = before.filter(|target| {
        focused_tabs_before.contains(target.tab_id.as_str())
            && !focused_tabs_after.contains(target.tab_id.as_str())
    });
    let gained = after.filter(|target| {
        !focused_tabs_before.contains(target.tab_id.as_str())
            && focused_tabs_after.contains(target.tab_id.as_str())
    });
    (lost, gained)
}

pub(super) fn forward_proxied_api_response(
    proxy: Option<(
        String,
        &'static str,
        std::sync::mpsc::Sender<shepr_api::error::ApiResult>,
        std::sync::mpsc::Receiver<shepr_api::error::ApiResult>,
    )>,
) -> Option<shepr_api::schema::ResponseResult> {
    let (request_id, method, respond_to, response_rx) = proxy?;
    let response = response_rx.recv().ok()?;
    let result = response.clone().ok();
    shepr_api::send_api_response(&respond_to, &request_id, method, response);
    result
}

impl HeadlessServer {
    pub(super) fn default_shell_target(&self) -> Option<crate::ui::TabSurfaceTarget> {
        let workspace_index = self.app.state.active_index()?;
        let workspace = self.app.state.workspaces.get(workspace_index)?;
        crate::ui::TabSurfaceTarget::from_indices(
            &self.app.state,
            workspace_index,
            workspace.active_tab_index(),
        )
    }

    pub(super) fn shell_target_for_client(
        &self,
        client_id: ClientId,
    ) -> Option<crate::ui::TabSurfaceTarget> {
        let tab_id = self
            .clients
            .get(&client_id)?
            .shell_state()?
            .location
            .as_ref()
            .and_then(crate::server::clients::ClientShellLocation::focused_tab_id);
        tab_id
            .and_then(|tab_id| self.app.resolve_tab_id(tab_id))
            .and_then(|(workspace_index, tab_index)| {
                crate::ui::TabSurfaceTarget::from_indices(
                    &self.app.state,
                    workspace_index,
                    tab_index,
                )
            })
            .or_else(|| self.default_shell_target())
    }

    fn tab_id_for_target(&self, target: &crate::ui::TabSurfaceTarget) -> Option<String> {
        target
            .resolve(&self.app.state)
            .map(|_| target.tab_id.to_string())
    }

    pub(super) fn shell_tab_id_for_client(&self, client_id: ClientId) -> Option<String> {
        self.shell_target_for_client(client_id)
            .and_then(|target| self.tab_id_for_target(&target))
    }

    fn client_shell_topology(&self) -> ClientShellTopology {
        let focused_workspace_id = self.app.state.active.clone();
        let fallback_workspace_id = self
            .app
            .state
            .workspaces
            .first()
            .map(|workspace| workspace.id.clone());
        let mut active_tab_ids = HashMap::new();
        let mut tab_workspace_ids = HashMap::new();
        for workspace in &self.app.state.workspaces {
            let workspace_id = workspace.id.clone();
            let active_tab_id = shepr_protocol::PublicTabId::new(
                workspace_id.as_str(),
                workspace.active_tab().number,
            );
            active_tab_ids.insert(workspace_id.clone(), active_tab_id);
            for tab in workspace.tabs() {
                let tab_id = shepr_protocol::PublicTabId::new(workspace_id.as_str(), tab.number);
                tab_workspace_ids.insert(tab_id, workspace_id.clone());
            }
        }
        ClientShellTopology {
            focused_workspace_id,
            fallback_workspace_id,
            active_tab_ids,
            tab_workspace_ids,
        }
    }

    pub(super) fn reconcile_client_shell_locations(&mut self) {
        let topology = self.client_shell_topology();
        let live_clients = self.clients.keys().copied().collect::<HashSet<_>>();
        self.clients
            .retain_geometry_controllers(|tab_id, client_id| {
                topology.tab_workspace_ids.contains_key(tab_id) && live_clients.contains(&client_id)
            });
        for client in self
            .clients
            .values_mut()
            .filter_map(|client| client.shell_state_mut())
        {
            let location = client.location.get_or_insert_with(|| {
                crate::server::clients::ClientShellLocation {
                    focused_workspace_id: topology.focused_workspace_id.clone(),
                    active_tab_ids: topology.active_tab_ids.clone(),
                }
            });
            location.reconcile(&topology);
        }
    }

    pub(super) fn focus_all_shell_clients_on_default_target(&mut self) {
        let Some(target) = self.default_shell_target() else {
            return;
        };
        let workspace_id = target.workspace_id.clone();
        let tab_id = target.tab_id.clone();
        let focus_before = self.shell_focus_targets();
        let focused_tabs_before = self.focused_shell_tabs();
        for client in self
            .clients
            .values_mut()
            .filter_map(|client| client.shell_state_mut())
        {
            if let Some(location) = client.location.as_mut() {
                location.focus_tab(workspace_id.clone(), tab_id.clone());
            }
        }
        let (lost, gained) =
            self.shell_location_focus_transitions(focus_before, &focused_tabs_before);
        self.app.accept_current_focus_with_api_events();
        self.send_shell_focus_transitions(&lost, &gained);
    }

    pub(super) fn focus_shell_client_on_tab(&mut self, client_id: ClientId, tab_id: &str) -> bool {
        let Some((workspace_index, _)) = self.app.parse_tab_id(tab_id) else {
            return false;
        };
        let Some(workspace_id) = self
            .app
            .state
            .workspaces
            .get(workspace_index)
            .map(|workspace| workspace.id.clone())
        else {
            return false;
        };
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let Some(location) = client
            .shell_state_mut()
            .and_then(|shell| shell.location.as_mut())
        else {
            return false;
        };
        let Ok(tab_id) = tab_id.parse() else {
            return false;
        };
        location.focus_tab(workspace_id, tab_id);
        true
    }

    pub(super) fn set_default_shell_target_from_client(&mut self, client_id: ClientId) -> bool {
        let Some(target) = self.shell_target_for_client(client_id) else {
            return false;
        };
        if self.default_shell_target().as_ref() == Some(&target) {
            return false;
        }
        let Some((workspace_index, tab_index)) = target.resolve(&self.app.state) else {
            return false;
        };
        let changed = self
            .app
            .state
            .switch_workspace_tab(workspace_index, tab_index);
        if changed {
            // The shared shell session snapshot includes default focus even
            // when this client's projection follows its own location.
            self.app.state.mark_shell_projection_dirty();
        }
        changed
    }

    pub(super) fn focus_shell_client_on_default_target(&mut self, client_id: ClientId) -> bool {
        let Some(tab_id) = self
            .default_shell_target()
            .and_then(|target| self.tab_id_for_target(&target))
        else {
            return false;
        };
        self.focus_shell_client_on_tab(client_id, &tab_id)
    }

    pub(super) fn apply_shell_navigation_request(
        &mut self,
        client_id: ClientId,
        method: &shepr_api::schema::Method,
    ) -> bool {
        match method {
            shepr_api::schema::Method::WorkspaceFocus(target) => {
                let Some(workspace_index) = self.app.parse_workspace_id(&target.workspace_id)
                else {
                    return false;
                };
                let Some(workspace_id) = self
                    .app
                    .state
                    .workspaces
                    .get(workspace_index)
                    .map(|workspace| workspace.id.clone())
                else {
                    return false;
                };
                let Some(client) = self.clients.get_mut(&client_id) else {
                    return false;
                };
                let Some(location) = client
                    .shell_state_mut()
                    .and_then(|shell| shell.location.as_mut())
                else {
                    return false;
                };
                location.focus_workspace(workspace_id);
                true
            }
            shepr_api::schema::Method::TabFocus(target) => {
                self.focus_shell_client_on_tab(client_id, &target.tab_id)
            }
            shepr_api::schema::Method::PaneFocus(target) => self
                .app
                .parse_pane_id(&target.pane_id)
                .and_then(|(workspace_index, pane_id)| {
                    let tab_index = self.app.state.workspaces[workspace_index]
                        .find_tab_index_for_pane(pane_id)?;
                    self.app.public_tab_id(workspace_index, tab_index)
                })
                .is_some_and(|tab_id| self.focus_shell_client_on_tab(client_id, &tab_id)),
            _ => false,
        }
    }

    fn focus_target_for_surface(
        &self,
        target: crate::ui::TabSurfaceTarget,
    ) -> Option<ShellFocusTarget> {
        let (workspace_index, tab_index) = target.resolve(&self.app.state)?;
        let pane_id = self
            .app
            .state
            .workspaces
            .get(workspace_index)?
            .tabs()
            .get(tab_index)?
            .layout
            .focused();
        Some(ShellFocusTarget {
            tab_id: target.tab_id,
            workspace_id: target.workspace_id,
            pane_id,
        })
    }

    pub(super) fn shell_focus_target(&self, client_id: ClientId) -> Option<ShellFocusTarget> {
        self.focus_target_for_surface(self.shell_target_for_client(client_id)?)
    }

    pub(super) fn focused_shell_tabs(&self) -> HashSet<String> {
        self.clients
            .iter()
            .filter(|(_, client)| {
                client.is_active_shell_client()
                    && client
                        .shell_state()
                        .is_some_and(|shell| shell.outer_terminal_focus == Some(true))
            })
            .filter_map(|(&client_id, _)| self.shell_tab_id_for_client(client_id))
            .collect()
    }

    pub(super) fn shell_focus_targets(&self) -> Vec<(ClientId, Option<ShellFocusTarget>)> {
        self.clients
            .iter()
            .filter(|(_, client)| client.is_active_shell_client())
            .map(|(&client_id, _)| (client_id, self.shell_focus_target(client_id)))
            .collect()
    }

    pub(super) fn send_shell_focus_target(
        &self,
        target: &ShellFocusTarget,
        event: shepr_vt::FocusEvent,
    ) {
        if let Some(workspace_index) = self
            .app
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == target.workspace_id)
        {
            self.app
                .send_pane_focus_event(workspace_index, target.pane_id, event);
        }
    }

    fn shell_location_focus_transitions(
        &self,
        focus_before: Vec<(ClientId, Option<ShellFocusTarget>)>,
        focused_tabs_before: &HashSet<String>,
    ) -> (
        HashMap<String, ShellFocusTarget>,
        HashMap<String, ShellFocusTarget>,
    ) {
        let focused_tabs_after = self.focused_shell_tabs();
        let mut lost = HashMap::<String, ShellFocusTarget>::new();
        let mut gained = HashMap::<String, ShellFocusTarget>::new();
        for (client_id, before) in focus_before {
            let after = self.shell_focus_target(client_id);
            let (lost_target, gained_target) = classify_shell_focus_transition(
                before.as_ref(),
                after.as_ref(),
                focused_tabs_before,
                &focused_tabs_after,
            );
            if let Some(target) = lost_target {
                lost.entry(target.tab_id.to_string())
                    .or_insert_with(|| target.clone());
            }
            if let Some(target) = gained_target {
                gained
                    .entry(target.tab_id.to_string())
                    .or_insert_with(|| target.clone());
            }
        }
        (lost, gained)
    }

    fn send_shell_focus_transitions(
        &self,
        lost: &HashMap<String, ShellFocusTarget>,
        gained: &HashMap<String, ShellFocusTarget>,
    ) {
        for target in lost.values() {
            self.send_shell_focus_target(target, shepr_vt::FocusEvent::Lost);
        }
        for target in gained.values() {
            self.send_shell_focus_target(target, shepr_vt::FocusEvent::Gained);
        }
    }

    pub(super) fn finish_shell_location_reconciliation(
        &mut self,
        focus_before: Vec<(ClientId, Option<ShellFocusTarget>)>,
        focused_tabs_before: &HashSet<String>,
    ) {
        let (lost, gained) =
            self.shell_location_focus_transitions(focus_before, focused_tabs_before);
        self.app.accept_current_focus_without_events();
        self.send_shell_focus_transitions(&lost, &gained);
    }

    pub(super) fn send_shell_navigation_focus_events(
        &self,
        before: Option<&ShellFocusTarget>,
        after: Option<&ShellFocusTarget>,
        focused_tabs_before: &HashSet<String>,
        focused_tabs_after: &HashSet<String>,
    ) {
        let (lost, gained) =
            classify_shell_focus_transition(before, after, focused_tabs_before, focused_tabs_after);
        if let Some(target) = lost {
            self.send_shell_focus_target(target, shepr_vt::FocusEvent::Lost);
        }
        if let Some(target) = gained {
            self.send_shell_focus_target(target, shepr_vt::FocusEvent::Gained);
        }
    }

    pub(super) fn shell_client_views_pane(
        &self,
        client_id: ClientId,
        workspace_index: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> bool {
        let Some(target) = self.shell_target_for_client(client_id) else {
            return false;
        };
        let Some((target_workspace_index, tab_index)) = target.resolve(&self.app.state) else {
            return false;
        };
        if target_workspace_index != workspace_index {
            return false;
        }
        let Some(tab) = self
            .app
            .state
            .workspaces
            .get(workspace_index)
            .and_then(|workspace| workspace.tabs().get(tab_index))
        else {
            return false;
        };
        if tab.zoomed {
            tab.layout.focused() == pane_id
        } else {
            tab.layout.pane_ids().contains(&pane_id)
        }
    }

    fn finish_shell_tab_geometry_change(&mut self, start_pending_agent_resumes: bool) {
        for client in self.clients.values_mut() {
            client.request_recompute();
        }
        if !start_pending_agent_resumes {
            self.app.pending_agent_resume_deadline = None;
            return;
        }
        let now = Instant::now();
        self.app.sync_pending_agent_resume_deadline(now);
        if self
            .app
            .start_pending_agent_resumes(now, self.app.pending_agent_resume_due(now))
        {
            for client in self.clients.values_mut() {
                client.request_recompute();
            }
        }
    }

    pub(super) fn apply_shell_tab_geometry(
        &mut self,
        client_id: ClientId,
        start_pending_agent_resumes: bool,
    ) -> bool {
        let Some(target) = self.shell_target_for_client(client_id) else {
            return false;
        };
        self.apply_shell_tab_geometry_to_target(client_id, target, start_pending_agent_resumes)
    }

    fn apply_shell_tab_geometry_to_target(
        &mut self,
        client_id: ClientId,
        target: crate::ui::TabSurfaceTarget,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if !self.resize_shell_tab_geometry_to_target(client_id, target) {
            return false;
        }
        self.finish_shell_tab_geometry_change(start_pending_agent_resumes);
        true
    }

    fn resize_shell_tab_geometry_to_target(
        &mut self,
        client_id: ClientId,
        target: crate::ui::TabSurfaceTarget,
    ) -> bool {
        let Some(client) = self.clients.get(&client_id) else {
            return false;
        };
        let (cols, rows) = (
            client.terminal_size.cols.get(),
            client.terminal_size.rows.get(),
        );
        let cell_size = client.cell_size.or_default();
        let area = Rect::new(0, 0, cols, rows);
        if self.app_client_count() == 1 {
            for (workspace_index, workspace) in self.app.state.workspaces.iter().enumerate() {
                for tab_index in 0..workspace.tabs().len() {
                    crate::ui::resize_tab_surface(
                        &self.app.state,
                        &self.app.terminal_runtimes,
                        workspace_index,
                        tab_index,
                        area,
                        cell_size,
                    );
                }
            }
        } else {
            let layout = crate::ui::compute_tab_surface_for(
                &self.app.state,
                &self.app.terminal_runtimes,
                Some(target),
                area,
            );
            crate::ui::resize_tab_surface_layout(
                &self.app.state,
                &self.app.terminal_runtimes,
                &layout,
                cell_size,
            );
        }
        true
    }

    pub(super) fn resize_tabs_for_only_shell_client(
        &mut self,
        start_pending_agent_resumes: bool,
    ) -> bool {
        let active_shell_count = self
            .clients
            .values()
            .filter(|client| client.is_active_shell_client() && client.writer.is_some())
            .count();
        if active_shell_count != 1 {
            return false;
        }
        let Some(client_id) = self.clients.iter().find_map(|(&client_id, client)| {
            (client.is_active_shell_client() && client.writer.is_some()).then_some(client_id)
        }) else {
            return false;
        };
        self.apply_shell_tab_geometry(client_id, start_pending_agent_resumes)
    }

    /// Resize unlocked panes to headless geometry when no shell controls their size.
    pub(super) fn resize_tabs_to_headless_size(&mut self, start_pending_agent_resumes: bool) {
        self.sync_foreground_client_state();
        let area = self.app.state.settings.headless_rect();
        crate::ui::resize_all_tab_surfaces(
            &self.app.state,
            &self.app.terminal_runtimes,
            area,
            shepr_termio::host_term::cell_size::HostCellSize::default(),
        );
        if start_pending_agent_resumes {
            self.finish_shell_tab_geometry_change(true);
        } else {
            // An attach departure leaves pending agent resumes as they are.
            for client in self.clients.values_mut() {
                client.request_recompute();
            }
        }
    }

    pub(super) fn reapply_controlled_shell_tab_geometry(
        &mut self,
        start_pending_agent_resumes: bool,
    ) -> bool {
        let mut viewed_tabs = HashMap::<String, Vec<ClientId>>::new();
        for (&client_id, client) in &self.clients {
            if !client.is_active_shell_client() || client.writer.is_none() {
                continue;
            }
            let Some(tab_id) = self.shell_tab_id_for_client(client_id) else {
                continue;
            };
            viewed_tabs.entry(tab_id).or_default().push(client_id);
        }
        for viewers in viewed_tabs.values_mut() {
            viewers.sort_unstable();
        }
        for (tab_id, viewers) in viewed_tabs {
            let controller_is_viewing = self
                .clients
                .geometry_controller(&tab_id)
                .as_ref()
                .is_some_and(|controller| viewers.contains(controller));
            if !controller_is_viewing {
                self.clients.set_geometry_controller(&tab_id, viewers[0]);
            }
        }

        if self.resize_tabs_for_only_shell_client(start_pending_agent_resumes) {
            return true;
        }

        let mut controlled_tabs = self
            .clients
            .geometry_controllers()
            .iter()
            .filter_map(|(tab_id, &client_id)| {
                self.app
                    .resolve_tab_id(tab_id)
                    .and_then(|(workspace_index, tab_index)| {
                        Some((
                            tab_id.clone(),
                            client_id,
                            crate::ui::TabSurfaceTarget::from_indices(
                                &self.app.state,
                                workspace_index,
                                tab_index,
                            )?,
                        ))
                    })
            })
            .collect::<Vec<_>>();
        controlled_tabs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let mut reapplied = false;
        for (_, client_id, target) in controlled_tabs {
            reapplied |= self.resize_shell_tab_geometry_to_target(client_id, target);
        }
        if reapplied {
            self.finish_shell_tab_geometry_change(start_pending_agent_resumes);
        }
        reapplied
    }

    pub(super) fn claim_shell_tab_geometry(
        &mut self,
        client_id: ClientId,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        let Some(tab_id) = self.shell_tab_id_for_client(client_id) else {
            return false;
        };
        if !self.clients.claim_geometry(&tab_id, client_id) {
            return false;
        }
        self.apply_shell_tab_geometry(client_id, start_pending_agent_resumes)
    }

    pub(super) fn claim_unowned_shell_tab_geometry(
        &mut self,
        client_id: ClientId,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        let Some(tab_id) = self.shell_tab_id_for_client(client_id) else {
            return false;
        };
        if !self.clients.claim_unowned_geometry(&tab_id, client_id) {
            return false;
        }
        self.apply_shell_tab_geometry(client_id, start_pending_agent_resumes)
    }

    pub(super) fn resize_shell_tab_if_controller(
        &mut self,
        client_id: ClientId,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        let Some(tab_id) = self.shell_tab_id_for_client(client_id) else {
            return false;
        };
        if self.clients.geometry_controller(&tab_id) != Some(client_id) {
            return false;
        }
        self.apply_shell_tab_geometry(client_id, start_pending_agent_resumes)
    }

    pub(super) fn shell_geometry_controller_for_terminal(
        &self,
        terminal_id: &str,
    ) -> Option<(ClientId, crate::ui::TabSurfaceTarget)> {
        let target = self.app.state.workspaces.iter().enumerate().find_map(
            |(workspace_index, workspace)| {
                workspace
                    .tabs()
                    .iter()
                    .enumerate()
                    .find_map(|(tab_index, tab)| {
                        tab.panes
                            .values()
                            .any(|pane| pane.attached_terminal_id.as_str() == terminal_id)
                            .then(|| {
                                crate::ui::TabSurfaceTarget::from_indices(
                                    &self.app.state,
                                    workspace_index,
                                    tab_index,
                                )
                            })
                            .flatten()
                    })
            },
        )?;
        self.clients
            .geometry_controller_by_id(&target.tab_id)
            .map(|client_id| (client_id, target))
    }

    pub(super) fn restore_shell_tab_geometry(
        &mut self,
        client_id: ClientId,
        target: crate::ui::TabSurfaceTarget,
    ) -> bool {
        self.apply_shell_tab_geometry_to_target(client_id, target, true)
    }
}
