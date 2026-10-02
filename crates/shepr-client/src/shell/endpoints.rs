use super::*;

#[derive(Clone, Debug)]
pub(crate) struct ClientShellEndpoint {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) status: ClientEndpointStatus,
    /// The cached endpoint and active projection share this immutable snapshot.
    pub(crate) snapshot: Option<Arc<ClientShellSnapshot>>,
    /// Connection generation that produced the snapshot; absent only in tests.
    pub(crate) snapshot_generation: Option<u64>,
    pub(crate) agent_recency: HashMap<shepr_protocol::PublicPaneId, u64>,
}

pub(super) struct MachineHit {
    pub(super) rect: Rect,
    pub(super) status_badge: Rect,
    pub(super) collapse_toggle: Rect,
    pub(super) endpoint_id: ClientEndpointId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEndpointFocusTarget {
    Workspace(shepr_protocol::WorkspaceId),
    Pane(shepr_protocol::PublicPaneId),
}

impl ClientShellState {
    /// Sets the configured machines, once at launch: Local first, then one endpoint per
    /// `[[machines]]` entry, each Connecting with no snapshot. The set never changes
    /// while the client runs.
    pub fn set_machines(&mut self, machines: &[shepr_config::MachineConfig]) {
        let mut next = Vec::with_capacity(machines.len().saturating_add(1));
        let local = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id.is_local())
            .cloned()
            .unwrap_or_else(local_endpoint);
        next.push(local);
        for machine in machines {
            next.push(ClientShellEndpoint {
                endpoint_id: ClientEndpointId::Ssh(machine.label.clone()),
                status: ClientEndpointStatus::Connecting,
                snapshot: None,
                snapshot_generation: None,
                agent_recency: HashMap::new(),
            });
        }
        self.endpoints = next;
        self.rebuild_endpoint_models();
    }

    pub fn set_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: ClientEndpointStatus,
    ) {
        if status == ClientEndpointStatus::Online {
            self.clear_machine_diagnostic(endpoint_id);
        }
        if endpoint_id == &self.active_endpoint_id && status != ClientEndpointStatus::Online {
            self.pending_workspace_highlight = None;
        }
        let mut changed = false;
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            changed = endpoint.status != status;
            endpoint.status = status;
        }
        if changed {
            self.rebuild_endpoint_models();
        }
    }

    pub(crate) fn mark_endpoint_disconnected(&mut self, endpoint_id: &ClientEndpointId) {
        self.set_endpoint_status(endpoint_id, ClientEndpointStatus::Reconnecting);
        if endpoint_id == &self.active_endpoint_id {
            self.drop_all_requests(DropReason::Interrupted);
        }
    }

    pub(crate) fn endpoint_projection_available(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints.iter().any(|endpoint| {
            &endpoint.endpoint_id == endpoint_id
                && endpoint.status == ClientEndpointStatus::Online
                && endpoint.snapshot.is_some()
        })
    }

    pub(crate) fn activate_endpoint_projection(&mut self, endpoint_id: &ClientEndpointId) -> bool {
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return false;
        };
        if endpoint.status != ClientEndpointStatus::Online {
            return false;
        }
        let Some(snapshot) = endpoint.snapshot.clone() else {
            return false;
        };
        let generation = endpoint.snapshot_generation;
        let pending_agent_reveal = self
            .pending_agent_reveal
            .take_if(|(target_endpoint, _)| target_endpoint == endpoint_id);
        let agent_body_height = self.hits.agent_body.height;
        let switching_endpoint = endpoint_id != &self.active_endpoint_id;
        let agent_scroll = self.agent_scroll;
        if switching_endpoint {
            self.active_endpoint_id = endpoint_id.clone();
            self.surfaces = PaneSurfaces::default();
        }
        self.apply_active_snapshot(snapshot, generation);
        if switching_endpoint {
            // The aggregate agent list belongs to the client, not one endpoint.
            self.agent_scroll = agent_scroll;
        }
        if let Some((_, pane_id)) = pending_agent_reveal {
            self.reveal_endpoint_agent(endpoint_id, &pane_id, agent_body_height);
        }
        true
    }

    pub(crate) fn endpoint_status(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<ClientEndpointStatus> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map(|endpoint| endpoint.status)
    }

    pub(crate) fn endpoint_has_snapshot(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| endpoint.snapshot.is_some())
    }

    pub(crate) fn endpoint_is_online(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoint_status(endpoint_id) == Some(ClientEndpointStatus::Online)
            && self.endpoint_has_snapshot(endpoint_id)
    }

    pub(crate) fn endpoint_boot_id(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<&shepr_protocol::BootId> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
            .snapshot
            .as_deref()
            .map(|snapshot| &snapshot.boot_id)
    }

    pub(crate) fn endpoint_snapshot_matches(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &shepr_protocol::BootId,
        revision: u64,
    ) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| {
                endpoint
                    .snapshot_generation
                    .is_none_or(|snapshot_generation| snapshot_generation == generation)
                    && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                        snapshot.boot_id == *boot_id && snapshot.revision == revision
                    })
            })
    }

    pub(crate) fn endpoint_snapshot_identity(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
    ) -> Option<(&shepr_protocol::BootId, u64)> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?;
        if endpoint
            .snapshot_generation
            .is_some_and(|snapshot_generation| snapshot_generation != generation)
        {
            return None;
        }
        endpoint
            .snapshot
            .as_deref()
            .map(|snapshot| (&snapshot.boot_id, snapshot.revision.get()))
    }

    /// A terminal normally starts focused. `None` means this host cannot report focus events,
    /// not that the endpoint has no viewer; a move's commit therefore sends an explicit true
    /// baseline.
    pub(crate) fn host_focus_baseline(&self) -> bool {
        self.outer_focused.unwrap_or(true)
    }

    pub(crate) fn active_endpoint_label(&self) -> &str {
        self.active_endpoint_id.display_label()
    }

    pub fn endpoint_is_active(&self, endpoint_id: &ClientEndpointId) -> bool {
        &self.active_endpoint_id == endpoint_id
    }

    pub(crate) fn multi_endpoint_active(&self) -> bool {
        self.endpoints.len() > 1
    }

    pub(crate) fn cache_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: impl Into<Arc<ClientShellSnapshot>>,
    ) {
        self.cache_endpoint_snapshot_at_generation(endpoint_id, Some(generation), snapshot.into());
    }

    fn cache_endpoint_snapshot_at_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
        snapshot: Arc<ClientShellSnapshot>,
    ) {
        let Some(index) = self
            .endpoints
            .iter()
            .position(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return;
        };
        if self.endpoints[index].snapshot_generation == generation
            && self.endpoints[index]
                .snapshot
                .as_deref()
                .is_some_and(|previous| {
                    previous.boot_id == snapshot.boot_id && previous.revision > snapshot.revision
                })
        {
            return;
        }
        let previous = self.endpoints[index].snapshot.as_deref();
        let previous_sequences = previous
            .into_iter()
            .flat_map(|snapshot| snapshot.agents.iter())
            .map(|agent| (agent.pane_id.as_str(), agent.state_change_seq))
            .collect::<HashMap<_, _>>();
        let mut next_recency = self
            .endpoints
            .iter()
            .flat_map(|endpoint| endpoint.agent_recency.values())
            .copied()
            .max()
            .unwrap_or_default();
        let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
        agents.sort_by_key(|agent| agent.state_change_seq);
        let mut recency = self.endpoints[index].agent_recency.clone();
        for agent in agents {
            let changed = previous_sequences
                .get(agent.pane_id.as_str())
                .is_none_or(|previous_seq| *previous_seq != agent.state_change_seq);
            if changed {
                next_recency = next_recency.saturating_add(1);
                recency.insert(agent.pane_id.clone(), next_recency);
            }
        }
        let live_agent_ids = snapshot
            .agents
            .iter()
            .map(|agent| &agent.pane_id)
            .collect::<std::collections::HashSet<_>>();
        recency.retain(|pane_id, _| live_agent_ids.contains(pane_id));
        let endpoint = &mut self.endpoints[index];
        endpoint.agent_recency = recency;
        endpoint.snapshot_generation = generation;
        endpoint.snapshot = Some(snapshot);
        self.rebuild_endpoint_models();
    }

    pub fn set_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }

    fn apply_cached_endpoint_snapshot(&mut self, endpoint_id: &ClientEndpointId) {
        let Some((snapshot, generation)) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                endpoint
                    .snapshot
                    .clone()
                    .map(|snapshot| (snapshot, endpoint.snapshot_generation))
            })
        else {
            return;
        };
        if endpoint_id == &self.active_endpoint_id {
            self.apply_active_snapshot(snapshot, generation);
        }
    }

    pub(super) fn rebuild_agent_panel_model(&mut self) {
        self.agent_panel_model =
            super::aggregate_navigation::AgentPanelModel::build(&self.endpoints, &self.config);
    }

    fn rebuild_endpoint_models(&mut self) {
        self.rebuild_agent_panel_model();
        self.navigator_index = super::aggregate_navigation::NavigatorIndex::build(&self.endpoints);
    }
}

pub(super) fn endpoint_status_presentation(
    status: ClientEndpointStatus,
    palette: &Palette,
) -> (&'static str, &'static str, ratatui::style::Color) {
    match status {
        ClientEndpointStatus::Connecting => ("◐", "connecting", palette.yellow),
        ClientEndpointStatus::Online => ("●", "online", palette.green),
        ClientEndpointStatus::Reconnecting => ("◐", "reconnecting", palette.yellow),
        ClientEndpointStatus::Attention => ("!", "attention", palette.red),
    }
}

pub(super) fn local_endpoint() -> ClientShellEndpoint {
    ClientShellEndpoint {
        endpoint_id: ClientEndpointId::Local,
        status: ClientEndpointStatus::Online,
        snapshot: None,
        snapshot_generation: None,
        agent_recency: HashMap::new(),
    }
}

#[cfg(test)]
impl ClientShellState {
    pub fn set_snapshot(&mut self, snapshot: Box<ClientShellSnapshot>) {
        let endpoint_id = self.active_endpoint_id.clone();
        self.set_endpoint_snapshot(&endpoint_id, snapshot);
    }

    pub(crate) fn cache_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: impl Into<Arc<ClientShellSnapshot>>,
    ) {
        self.cache_endpoint_snapshot_at_generation(endpoint_id, None, snapshot.into());
    }

    pub fn set_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot(endpoint_id, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }
}
